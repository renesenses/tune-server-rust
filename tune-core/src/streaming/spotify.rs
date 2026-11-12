use reqwest::Client;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tracing::{info, warn};

use super::traits::*;
use crate::TuneError;

const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const API_BASE: &str = "https://api.spotify.com/v1";
const DEFAULT_CLIENT_ID: &str = "placeholder";
/// #7659 — le plus grand `limit` que `/v1/search` accepte d'une application
/// en quota étendu. Au-delà, Spotify répond `400 Invalid limit` : la limite
/// du client web (100, `SEARCH_FEDEREE_LIMIT`) partait telle quelle et la
/// recherche ne rendait rien.
const PLAFOND_RECHERCHE: usize = 50;
/// #7659 — le plafond d'une application en mode développement, celle que
/// chaque utilisateur crée pour coller son Client ID (fil 221), depuis les
/// changements de l'API Web de février 2026. Il n'est pas supposé d'avance :
/// le service l'adopte au premier `400 Invalid limit` qu'il reçoit à 50
/// (voir [`SpotifyService::recherche`]).
const PLAFOND_RECHERCHE_MODE_DEVELOPPEMENT: usize = 10;
/// #8479 — le plus grand `limit` que `GET /artists/{id}/albums` accepte
/// depuis février 2026 (0 à 10). Au-delà : `400 Invalid limit`.
const PAGE_ALBUMS_ARTISTE: usize = 10;
/// #8479 — borne de sécurité de la discographie : 30 pages de 10, soit 300
/// albums et singles, sur un service qui annoncerait toujours une suite.
const PAGES_ALBUMS_ARTISTE_MAX: usize = 30;
// Spotify n'accepte plus l'alias `localhost` : le HTTP en clair n'est admis
// que pour un LITTERAL de boucle locale. C'est acquis depuis #2680 et ce
// n'est pas ce qui change ici.
//
// Ce qui change, c'est le PORT. Il etait ecrit en dur a 8888 — le defaut de
// Tune — alors que `TUNE_PORT` le deplace et que le serveur ecoute la ou on
// le lui dit (`tune-server/src/bootstrap.rs:289`, `config.port`). Une
// installation sur un autre port envoyait donc a Spotify une URI nommant un
// port ou rien ne repond : l'autorisation aboutit et retombe sur le vide.
// C'est exactement le doute que Krugy a formule sur le fil 34 — « le port
// Tune est bien 8888 sur mon installe » — face a un defaut qui, a l'epoque,
// en nommait un autre. Le port se DERIVE de celui qu'on ecoute.
//
// Cette valeur doit par ailleurs etre declaree A L'IDENTIQUE dans le tableau
// de bord de l'application Spotify de l'utilisateur, et Tune ne peut pas le
// faire a sa place : `GET /api/v1/system/env` la publie donc telle qu'elle
// partira, pour qu'elle soit recopiable sans la deviner.
//
// Une installation distante reste tenue de fournir son URI HTTPS enregistree
// (configuration explicite ou variable) : rien ici ne la devine.
pub const DEFAULT_API_PORT: u16 = 8888;
const REDIRECT_PATH: &str = "/api/v1/streaming/spotify/callback";

/// L'URI de redirection par defaut d'un serveur qui ecoute sur `api_port`.
pub fn default_redirect_uri(api_port: u16) -> String {
    format!("http://127.0.0.1:{api_port}{REDIRECT_PATH}")
}
/// #6018 — `streaming` connecte librespot au compte ; les deux portées
/// `*-playback-state` laissent l'API Web désigner et piloter son appareil.
/// Un compte relié AVANT ce changement doit se reconnecter une fois.
const SCOPES: &str = "user-read-private user-library-read playlist-read-private playlist-modify-private playlist-modify-public streaming user-read-playback-state user-modify-playback-state";

pub struct SpotifyService {
    client: Client,
    client_id: String,
    access_token: Option<String>,
    refresh_token: Option<String>,
    username: Option<String>,
    user_id: Option<String>,
    /// Fil 221 — les autorisations entamees et pas encore echangees, la plus
    /// recente en dernier. Chacune garde le `state` envoye a Spotify et le
    /// verificateur PKCE dont le `code_challenge` a ete envoye avec.
    tentatives: Vec<TentativePkce>,
    redirect_uri: String,
    /// `TOKEN_URL` et `API_BASE`, sauf en test ou une doublure locale repond.
    token_url: String,
    api_base: String,
    token_expires: Option<std::time::Instant>,
    enabled_override: Option<bool>,
    /// Posé par un appel HTTP qui s'est vu refuser le jeton (401).
    ///
    /// Les quatre méthodes HTTP prennent `&self` : elles ne peuvent pas
    /// rafraîchir sur place comme le fait Tidal (dont l'état de jetons vit
    /// derrière un `Mutex`). Ce drapeau est le relais : il transporte le refus
    /// jusqu'au prochain `refresh_if_needed`, qui a `&mut self`. Sans lui, un
    /// jeton révoqué produisait des 401 indéfiniment sans que rien ne tente
    /// de le renouveler.
    token_rejected: Arc<AtomicBool>,
    /// #6933 — les portées que Spotify dit avoir ACCORDÉES (champ `scope`
    /// de la réponse du jeton, à l'échange comme au rafraîchissement).
    /// `None` : inconnues (liaison restaurée d'une version qui ne les gardait
    /// pas, pas encore rafraîchie).
    portees: Option<String>,
    /// #7659 — le `limit` de recherche que Spotify accepte de CETTE
    /// application : [`PLAFOND_RECHERCHE`], ramené à
    /// [`PLAFOND_RECHERCHE_MODE_DEVELOPPEMENT`] après un refus de limite.
    /// Atomique pour la même raison que `token_rejected`.
    plafond_recherche: AtomicUsize,
    /// #8051 — Spotify a refusé pour de bon le renouvellement du jeton
    /// (`400 invalid_grant`, `401`) : la session en mémoire est effacée et
    /// [`StreamingService::session_expired`] le dit au rafraîchisseur, qui
    /// supprime la ligne de jetons. Sans cela, le service restait annoncé
    /// « Connecté » avec un jeton mort, et chaque recherche échouait.
    session_refusee: bool,
    /// #8198 — le dernier refus de connexion portant un code stable, efface
    /// par une connexion reussie. Le verificateur (`diagnostic`) le rapporte.
    derniere_erreur: Option<DerniereErreur>,
    /// #8198 — l'adresse du relais de retour quand l'option « retour
    /// automatique » est active ; `None` (defaut) : l'adresse de retour reste
    /// `redirect_uri`, comme avant.
    relais: Option<String>,
    /// #8485 — Spotify a répondu 403 à `/artists/{id}/top-tracks` : depuis
    /// février 2026, ce point d'accès est retiré aux applications en mode
    /// développement (celle de chaque utilisateur). Les titres phares sont
    /// alors vides, sans erreur, et ne sont plus redemandés à Spotify.
    titres_phares_retires: AtomicBool,
    /// #8399 — Spotify a répondu 403 à `/browse/featured-playlists` : ce
    /// point d'accès est fermé aux applications créées après le 27/11/2024.
    /// La rubrique est alors vide, sans erreur, et n'est plus redemandée.
    selection_retiree: AtomicBool,
    /// Février 2026 — Spotify a retiré `GET /browse/new-releases` aux
    /// applications en mode développement : la rubrique « Nouveautés » est
    /// alors vide, sans erreur, et n'est plus redemandée.
    nouveautes_retirees: AtomicBool,
    /// Février 2026 — `GET /browse/categories` et
    /// `/browse/categories/{id}/…` retirés de même : les genres sont vides.
    genres_retires: AtomicBool,
}

impl Default for SpotifyService {
    fn default() -> Self {
        Self::new()
    }
}

/// Ordre de priorite, inchange : configuration explicite, puis
/// `TUNE_SPOTIFY_REDIRECT_URI`, puis l'ancienne `SPOTIFY_REDIRECT_URI`, puis
/// le defaut derive du port ecoute.
pub fn resolve_redirect_uri(
    explicit: Option<&str>,
    tune_env: Option<String>,
    legacy_env: Option<String>,
    api_port: u16,
) -> String {
    explicit
        .filter(|uri| !uri.is_empty())
        .map(str::to_owned)
        .or_else(|| tune_env.filter(|uri| !uri.is_empty()))
        .or_else(|| legacy_env.filter(|uri| !uri.is_empty()))
        .unwrap_or_else(|| default_redirect_uri(api_port))
}

/// Pourquoi Spotify refusera une URI de redirection — #2680, fil 221.
///
/// fne mcnp revient le 25/09/2026 avec le libelle exact : « redirect_uri:
/// Insecure ». La regle Spotify (« Redirect URIs », en vigueur depuis le
/// 9 avril 2025) : HTTPS obligatoire, SAUF un litteral de boucle locale
/// explicite `http://127.0.0.1:<port>` ou `http://[::1]:<port>` ; `localhost`
/// est refuse. Le defaut de Tune y est conforme, mais une URI EXPLICITE
/// (configuration, `TUNE_SPOTIFY_REDIRECT_URI`, `SPOTIFY_REDIRECT_URI`) etait
/// prise telle quelle sans rien dire — or le conseil publie sur ce meme fil
/// le 19/05 etait precisement `http://VOTRE_IP:8888/...`, que Spotify refuse.
///
/// La valeur de l'exploitant est CONSERVEE (Tune ne reecrit jamais une URI
/// declaree a l'identique dans un tableau de bord tiers) ; elle est seulement
/// NOMMEE comme refusee, dans le journal et dans `GET /system/env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusRedirection {
    /// `http://localhost...` : Spotify n'accepte plus l'alias.
    Localhost,
    /// `http://` vers un hote qui n'est pas un litteral de boucle locale
    /// (IP du reseau local, nom d'hote) : « Insecure ».
    HttpHorsBouclage,
}

impl RefusRedirection {
    pub fn code(self) -> &'static str {
        match self {
            Self::Localhost => "localhost",
            Self::HttpHorsBouclage => "http_hors_bouclage",
        }
    }
}

/// Classe une URI de redirection selon la regle Spotify. `None` : acceptable.
///
/// HTTPS passe toujours ; un schema propre a une application (`monappli://`)
/// n'est pas du ressort de cette regle et passe aussi.
pub fn refus_redirection(uri: &str) -> Option<RefusRedirection> {
    let uri = uri.trim();
    let (schema, reste) = uri.split_once("://")?;
    if !schema.eq_ignore_ascii_case("http") {
        return None;
    }
    let autorite = reste.split(['/', '?', '#']).next().unwrap_or("");
    let autorite = autorite.rsplit('@').next().unwrap_or(autorite);
    let hote = if autorite.starts_with('[') {
        match autorite.find(']') {
            Some(fin) => &autorite[..=fin],
            None => autorite,
        }
    } else {
        autorite.split(':').next().unwrap_or(autorite)
    }
    .to_ascii_lowercase();
    match hote.as_str() {
        "127.0.0.1" | "[::1]" => None,
        "localhost" => Some(RefusRedirection::Localhost),
        h if h.ends_with(".localhost") => Some(RefusRedirection::Localhost),
        _ => Some(RefusRedirection::HttpHorsBouclage),
    }
}

/// Le `code` d'autorisation contenu dans l'adresse de rappel COLLEE par
/// l'utilisateur — #2680.
///
/// Le rappel vise `127.0.0.1` : il n'aboutit que dans le navigateur de la
/// machine du serveur. Depuis un autre poste, Spotify renvoie ce navigateur
/// vers une page qui ne s'ouvre pas — mais l'adresse, dans la barre, porte le
/// `code`. La coller dans Tune termine l'echange (meme verificateur PKCE, meme
/// `redirect_uri`). Un code nu est accepte tel quel.
pub fn code_depuis_rappel(colle: &str) -> Result<String, String> {
    rappel_depuis_colle(colle)
        .map(|r| r.code)
        .map_err(|e| e.to_string())
}

/// #8142 — l'adresse collee ne porte pas de `code` : c'est souvent l'adresse
/// de redirection recopiee du tableau de bord, pas celle de la page 127.0.0.1
/// apres le consentement.
pub const CODE_CALLBACK_SANS_CODE: &str = "spotify_callback_sans_code";

/// #8142 — l'adresse collee porte `error=access_denied` : l'utilisateur a
/// refuse l'acces sur la page Spotify.
pub const CODE_AUTORISATION_REFUSEE: &str = "spotify_autorisation_refusee";

/// #8142 — le `state` de l'adresse collee ne designe aucune autorisation en
/// cours (autre onglet, lien ancien, serveur redemarre).
pub const CODE_STATE_INCONNU: &str = "spotify_state_inconnu";

/// #8198 — aucun Client ID configure sur ce serveur.
pub const CODE_CLIENT_ID_ABSENT: &str = "spotify_client_id_absent";

/// #8198 — `invalid_grant` a l'echange : code deja utilise, expire ou faux.
pub const CODE_CODE_INVALIDE: &str = "spotify_code_invalide";

/// #8198 — l'adresse de retour declaree dans l'application Spotify ne
/// correspond pas a celle que Tune envoie.
pub const CODE_REDIRECT_URI_DIFFERENTE: &str = "spotify_redirect_uri_differente";

/// #8198 — `invalid_client` : Client ID inconnu de Spotify.
pub const CODE_CLIENT_ID_REFUSE: &str = "spotify_client_id_refuse";

/// #8198 — Spotify a delivre un jeton, mais `/me` repond 403 : compte absent
/// de « User Management », ou proprietaire de l'application sans Premium
/// (regle du mode developpement depuis fevrier 2026). Sans ce controle, la
/// carte affichait « connecte » et tout echouait ensuite.
pub const CODE_COMPTE_NON_AUTORISE: &str = "spotify_compte_non_autorise";

const COMPTE_NON_AUTORISE: &str = "spotify: Spotify refuse l'acces a ce compte (HTTP 403). \
     Ajoutez l'adresse e-mail de votre compte Spotify dans User Management de votre \
     application (developer.spotify.com, tableau de bord), et verifiez que le \
     proprietaire de l'application a un abonnement Premium";

/// #8198 — le code stable d'un refus de l'echange du code (`/api/token`).
///
/// Spotify repond `{"error": "invalid_grant", "error_description": "Invalid
/// redirect URI"}` quand l'adresse de retour differe de celle de
/// l'autorisation, `invalid_grant` seul pour un code use, expire ou faux, et
/// `invalid_client` pour un Client ID inconnu.
pub fn code_du_refus_d_echange(erreur: &str, description: &str) -> Option<&'static str> {
    let description = description.to_ascii_lowercase();
    if description.contains("redirect") {
        return Some(CODE_REDIRECT_URI_DIFFERENTE);
    }
    match erreur {
        "invalid_grant" => Some(CODE_CODE_INVALIDE),
        "invalid_client" => Some(CODE_CLIENT_ID_REFUSE),
        _ => None,
    }
}

/// #8198 — un echec d'appel a l'API Web, avec le statut HTTP quand Spotify
/// a repondu. Le statut se lit ici, pas dans le texte du message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErreurApi {
    /// `None` : pas de reponse (reseau, delai depasse) ou corps illisible.
    pub statut: Option<u16>,
    pub message: String,
}

/// #8198 — statut de `/me` pour un compte que l'application n'autorise pas.
pub const STATUT_COMPTE_NON_AUTORISE: u16 = 403;

impl ErreurApi {
    fn sans_statut(message: impl Into<String>) -> Self {
        Self {
            statut: None,
            message: message.into(),
        }
    }

    /// Spotify refuse l'acces a ce compte (`403`) ?
    pub fn compte_non_autorise(&self) -> bool {
        self.statut == Some(STATUT_COMPTE_NON_AUTORISE)
    }

    /// Le jeton est refuse (`401`) ?
    pub fn jeton_refuse(&self) -> bool {
        self.statut == Some(401)
    }
}

/// #8198 — delai d'une sonde du verificateur (`/api/token` factice, `/me`) :
/// une page de diagnostic ne doit pas attendre un Spotify muet.
pub const DELAI_SONDE_DIAGNOSTIC: std::time::Duration = std::time::Duration::from_secs(5);

/// Un `GET` de l'API Web avec `jeton`. `401` donne `statut: Some(401)` et le
/// message `token expired` ; l'appelant note le refus du jeton.
async fn requete_api(
    client: &Client,
    api_base: &str,
    jeton: &str,
    path: &str,
    delai: Option<std::time::Duration>,
) -> Result<serde_json::Value, ErreurApi> {
    let mut requete = client
        .get(format!("{api_base}{path}"))
        .header("Authorization", format!("Bearer {jeton}"));
    if let Some(delai) = delai {
        requete = requete.timeout(delai);
    }
    let resp = requete
        .send()
        .await
        .map_err(|e| ErreurApi::sans_statut(format!("spotify api: {e}")))?;

    if resp.status() == 401 {
        return Err(ErreurApi {
            statut: Some(401),
            message: "token expired".into(),
        });
    }
    // #7659 et #6825 — un refus (400, 403, 404, 429…) porte un corps JSON
    // valide, `{"error":{"status":400,"message":"Invalid limit"}}` : le
    // lire comme une réponse faisait passer le refus pour une recherche
    // sans résultat ou un onglet Éditorial vide (mis en cache 30 min).
    // C'est une erreur, journalisée avec son statut ; le chemin est
    // journalisé sans sa requête, le motif borné à 200 caractères.
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let corps = resp.text().await.unwrap_or_default();
        let message = motif_du_refus(&corps);
        let route = path.split('?').next().unwrap_or(path);
        warn!(status, route, message = %message, "spotify_api_error");
        return Err(ErreurApi {
            statut: Some(status),
            message: format!("spotify GET {route}: HTTP {status} — {message}"),
        });
    }
    resp.json()
        .await
        .map_err(|e| ErreurApi::sans_statut(format!("spotify json: {e}")))
}

/// #8198 — le dernier refus de connexion, que le verificateur rapporte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerniereErreur {
    pub code: &'static str,
    pub quand: std::time::SystemTime,
}

fn refus(code: &'static str, message: impl Into<String>) -> TuneError {
    TuneError::Refused {
        code,
        message: message.into(),
    }
}

const SANS_CODE: &str = "spotify: l'adresse collee ne contient pas de code";

/// Fil 221 — au plus tant d'autorisations entamees gardees en memoire : deux
/// onglets, ou un second clic sur « Se connecter », ne s'ecrasent plus.
const TENTATIVES_MAX: usize = 4;

const AUCUNE_TENTATIVE: &str = "spotify: aucune connexion en cours (no code verifier) — \
     le serveur a pu redemarrer depuis l'ouverture du lien. Cliquez de nouveau sur \
     Se connecter, puis collez la nouvelle adresse de retour";

const TENTATIVE_INCONNUE: &str = "spotify: cette adresse de retour vient d'une autre \
     tentative de connexion (ou d'avant un redemarrage du serveur). Cliquez de nouveau \
     sur Se connecter, puis collez la nouvelle adresse de retour";

/// Une autorisation entamee : le `state` envoye et son verificateur PKCE.
struct TentativePkce {
    state: String,
    verifier: String,
    /// #8198 — l'adresse de retour envoyee avec CETTE autorisation : l'echange
    /// doit la renvoyer a l'identique, meme si le relais a ete active ou
    /// coupe entre-temps.
    redirect_uri: String,
    /// #8198 — secondes Unix de l'ouverture ; au-dela de
    /// [`DUREE_TENTATIVE_SECS`], la tentative est oubliee.
    ouverte_le: u64,
}

/// #8198 — l'adresse du relais de retour de Mozaik Labs. Une adresse HTTPS
/// publique et fixe, la meme pour tous : chacun la declare une fois dans SA
/// propre application Spotify. Le relais renvoie le navigateur vers
/// l'installation Tune dont l'origine voyage dans le `state` ; le code seul ne
/// sert a rien sans le verificateur PKCE, qui ne quitte jamais ce serveur.
pub const RELAIS_PAR_DEFAUT: &str = "https://mozaiklabs.fr/spotify-callback";

/// #8198 — l'adresse du relais : `TUNE_SPOTIFY_RELAIS_URL` (essais), sinon
/// [`RELAIS_PAR_DEFAUT`].
pub fn adresse_du_relais() -> String {
    std::env::var("TUNE_SPOTIFY_RELAIS_URL")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| RELAIS_PAR_DEFAUT.to_owned())
}

/// #8198 — duree de vie d'une autorisation entamee (persistee ou non).
pub const DUREE_TENTATIVE_SECS: u64 = 15 * 60;

fn maintenant_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// #8198 — encode un composant d'URL (requete de l'adresse de retour).
pub fn encoder_composant(texte: &str) -> String {
    urlencoding::encode(texte).into_owned()
}

/// #8198 — decode un composant d'URL ; le texte tel quel s'il est invalide.
pub fn decoder_composant(texte: &str) -> String {
    urlencoding::decode(texte)
        .map(|v| v.into_owned())
        .unwrap_or_else(|_| texte.to_owned())
}

/// #8198 — l'origine de la page Tune qui lance la connexion, reduite a
/// `schema://hote[:port]`. `None` si elle n'a pas cette forme : pas
/// d'identifiants, pas d'espace, un hote non vide (nom ASCII ou IPv6 entre
/// crochets), un port numerique de 1 a 65535, http(s) seulement, 200 caracteres au plus.
/// Le relais du site refait ses propres controles ; c'est le nonce du
/// `state`, connu de ce seul serveur, qui fait foi au retour.
pub fn normaliser_origine(origine: &str) -> Option<String> {
    let o = origine.trim();
    if o.len() > 200 || o.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (schema, reste) = o.split_once("://")?;
    let schema = schema.to_ascii_lowercase();
    if schema != "http" && schema != "https" {
        return None;
    }
    let autorite = reste.split(['/', '?', '#']).next().unwrap_or("");
    if autorite.is_empty() || autorite.contains('@') {
        return None;
    }
    let (hote_valide, port) = match autorite.strip_prefix('[') {
        Some(reste) => {
            let (ip6, apres) = reste.split_once(']')?;
            (ip6.parse::<std::net::Ipv6Addr>().is_ok(), apres)
        }
        None => {
            let (hote, apres) = autorite.split_at(autorite.find(':').unwrap_or(autorite.len()));
            let valide = !hote.is_empty()
                && hote
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
            (valide, apres)
        }
    };
    if !hote_valide {
        return None;
    }
    if !port.is_empty() {
        let chiffres = port.strip_prefix(':')?;
        if chiffres.is_empty()
            || chiffres.len() > 5
            || !chiffres.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        if !(1..=65535).contains(&chiffres.parse::<u32>().ok()?) {
            return None;
        }
    }
    Some(format!("{schema}://{}", autorite.to_ascii_lowercase()))
}

/// Ce qu'une adresse de retour apporte : le code, et le `state` s'il y est.
#[derive(Debug, PartialEq, Eq)]
pub struct RappelSpotify {
    pub code: String,
    pub state: Option<String>,
}

fn octets_aleatoires<const N: usize>() -> [u8; N] {
    let mut octets = [0u8; N];
    getrandom::getrandom(&mut octets).expect("OS RNG unavailable");
    octets
}

/// Comme [`code_depuis_rappel`], en gardant le `state` de l'adresse collee.
pub fn rappel_depuis_colle(colle: &str) -> Result<RappelSpotify, TuneError> {
    let colle = colle.trim();
    if colle.is_empty() {
        return Err("spotify: adresse de rappel vide".into());
    }
    let Some((_, requete)) = colle.split_once('?') else {
        if colle.contains("://") || colle.contains('/') {
            return Err(refus(CODE_CALLBACK_SANS_CODE, SANS_CODE));
        }
        // Fil 221 — Yan Tasset y a colle son Client ID : il partait vers
        // `api/token` comme un code, et Spotify repondait `invalid_client`.
        if ressemble_a_un_client_id(colle) {
            return Err(
                "spotify: ceci est un Client ID (32 caracteres hexadecimaux), \
                        pas l'adresse de retour. Enregistrez-le dans le champ Client ID, \
                        puis collez ici l'adresse complete de la page 127.0.0.1 \
                        (elle contient ?code=...)"
                    .into(),
            );
        }
        return Ok(RappelSpotify {
            code: colle.to_owned(),
            state: None,
        });
    };
    let requete = requete.split('#').next().unwrap_or(requete);
    let mut erreur = None;
    let mut code = None;
    let mut state = None;
    for paire in requete.split('&') {
        let (cle, valeur) = paire.split_once('=').unwrap_or((paire, ""));
        let valeur = urlencoding::decode(valeur)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| valeur.to_owned());
        match cle {
            "code" if !valeur.is_empty() => code = Some(valeur),
            "state" if !valeur.is_empty() => state = Some(valeur),
            "error" => erreur = Some(valeur),
            _ => {}
        }
    }
    if let Some(code) = code {
        return Ok(RappelSpotify { code, state });
    }
    Err(match erreur.as_deref() {
        Some("access_denied") => refus(CODE_AUTORISATION_REFUSEE, "spotify: access_denied"),
        Some(e) => format!("spotify: {e}").into(),
        None => refus(CODE_CALLBACK_SANS_CODE, SANS_CODE),
    })
}

/// Fil 221 (Yan Tasset) — debut du refus d'`authenticate` quand le serveur
/// n'a aucun Client ID : la route de streaming et l'ecran le reconnaissent.
pub const CLIENT_ID_ABSENT: &str = "spotify: aucun Client ID configure";

/// Un Client ID est-il REELLEMENT configure ? Vide et `"placeholder"` (le
/// defaut de la caisse) ne le sont pas : avec eux, Spotify repond
/// `invalid_client` a tout.
pub fn client_id_configure(client_id: &str) -> bool {
    let id = client_id.trim();
    !id.is_empty() && id != DEFAULT_CLIENT_ID
}

/// Fil 221 — un Client ID Spotify est une suite de 32 caracteres
/// hexadecimaux. Un code d'autorisation, lui, en compte bien davantage et
/// n'est jamais de cette forme : un texte nu de cette forme colle dans le
/// champ de l'adresse de retour est un Client ID, pas un code.
pub fn ressemble_a_un_client_id(texte: &str) -> bool {
    let t = texte.trim();
    t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// #8198 — le Client ID tire de ce que l'utilisateur a colle.
///
/// Vu en direct avec un testeur : le champ a recu l'adresse du tableau de
/// bord Spotify au lieu du Client ID, et Spotify repondait `INVALID_CLIENT:
/// Invalid client`. L'identifiant de cette adresse N'EST PAS le Client ID :
/// rien n'en est tire. Seuls 32 caracteres hexadecimaux sont acceptes
/// (espaces autour toleres) ; `None` pour tout le reste.
pub fn client_id_depuis_saisie(saisie: &str) -> Option<String> {
    let t = saisie.trim();
    ressemble_a_un_client_id(t).then(|| t.to_owned())
}

/// #8198 — la saisie est-elle une adresse du tableau de bord Spotify ? Le
/// refus le dit alors precisement.
pub fn est_une_adresse_du_tableau_de_bord(saisie: &str) -> bool {
    saisie
        .to_ascii_lowercase()
        .contains("developer.spotify.com/dashboard")
}

/// Le Client ID que le service doit employer — fil 221.
///
/// Ordre de priorite, du plus fort au plus faible :
/// 1. la variable `TUNE_SPOTIFY_CLIENT_ID` (l'exploitant l'impose) ;
/// 2. le reglage `spotify_client_id` en base, saisi dans Reglages ;
/// 3. `spotify_client_id` de `tune.toml`.
///
/// `None` : rien de tout cela. `with_config` se rabat alors sur l'ancienne
/// variable `SPOTIFY_CLIENT_ID`, puis sur `"placeholder"` (non configure).
/// Une valeur vide ou egale a `"placeholder"` compte comme absente.
pub fn resolve_client_id(
    tune_env: Option<&str>,
    reglage: Option<&str>,
    toml: Option<&str>,
) -> Option<String> {
    [tune_env, reglage, toml]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|id| client_id_configure(id))
        .map(str::to_owned)
}

/// L'URI que Tune enverra REELLEMENT a Spotify, lue depuis l'environnement du
/// processus — la meme resolution que celle appliquee a la construction du
/// service, au meme endroit, pour qu'une page de support ne puisse pas en
/// afficher une autre (#2680).
pub fn effective_redirect_uri(explicit: Option<&str>, api_port: u16) -> String {
    resolve_redirect_uri(
        explicit,
        std::env::var("TUNE_SPOTIFY_REDIRECT_URI").ok(),
        std::env::var("SPOTIFY_REDIRECT_URI").ok(),
        api_port,
    )
}

impl SpotifyService {
    pub fn new() -> Self {
        Self::with_config(None, None, DEFAULT_API_PORT)
    }

    /// Create a SpotifyService with explicit client_id and redirect_uri from
    /// TuneConfig. `api_port` est le port REELLEMENT ecoute : il ne sert qu'au
    /// defaut, et n'ecrase jamais une URI explicite (#2680).
    pub fn with_config(client_id: Option<&str>, redirect_uri: Option<&str>, api_port: u16) -> Self {
        let client_id = client_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                std::env::var("TUNE_SPOTIFY_CLIENT_ID")
                    .or_else(|_| std::env::var("SPOTIFY_CLIENT_ID"))
                    .unwrap_or_else(|_| DEFAULT_CLIENT_ID.into())
            });
        let redirect_uri = effective_redirect_uri(redirect_uri, api_port);
        if let Some(refus) = refus_redirection(&redirect_uri) {
            warn!(
                redirect_uri = %redirect_uri,
                refus = refus.code(),
                "spotify_redirect_uri_refusee: Spotify n'accepte en http:// que \
                 http://127.0.0.1:<port> ou http://[::1]:<port> ; sinon HTTPS"
            );
        }
        Self {
            client: crate::http::client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            client_id,
            access_token: None,
            refresh_token: None,
            username: None,
            user_id: None,
            tentatives: Vec::new(),
            redirect_uri,
            token_url: TOKEN_URL.to_owned(),
            api_base: API_BASE.to_owned(),
            token_expires: None,
            enabled_override: None,
            token_rejected: Arc::new(AtomicBool::new(false)),
            portees: None,
            plafond_recherche: AtomicUsize::new(PLAFOND_RECHERCHE),
            session_refusee: false,
            derniere_erreur: None,
            relais: None,
            titres_phares_retires: AtomicBool::new(false),
            selection_retiree: AtomicBool::new(false),
            nouveautes_retirees: AtomicBool::new(false),
            genres_retires: AtomicBool::new(false),
        }
    }

    /// Fil 221 — le Client ID saisi dans Reglages, applique a chaud. Un
    /// echange PKCE entame avec l'ancien identifiant ne peut pas aboutir avec
    /// le nouveau : son verificateur est oublie.
    pub fn set_client_id(&mut self, client_id: &str) {
        self.client_id = client_id.trim().to_owned();
        // #8485 — le retrait des titres phares vaut pour UNE application.
        self.titres_phares_retires.store(false, Ordering::Relaxed);
        // #8399 — le refus de la sélection vaut pour UNE application.
        self.selection_retiree.store(false, Ordering::Relaxed);
        self.nouveautes_retirees.store(false, Ordering::Relaxed);
        self.genres_retires.store(false, Ordering::Relaxed);
        self.tentatives.clear();
    }

    /// Fil 221 — la tentative dont un code de retour releve.
    ///
    /// Le `state` du retour la designe ; un code colle nu (sans `state`)
    /// revient a la plus recente. Rien n'est consomme ici : la tentative ne
    /// s'efface qu'apres un echange reussi.
    ///
    /// #8142 — un `state` qui ne designe aucune tentative, y compris quand il
    /// n'y en a plus aucune en memoire (serveur redemarre), porte
    /// [`CODE_STATE_INCONNU`] ; un code nu sans tentative garde son texte seul.
    fn tentative_pour(&self, state: Option<&str>) -> Result<usize, TuneError> {
        // #8198 — une tentative de plus de `DUREE_TENTATIVE_SECS` ne compte plus.
        let maintenant = maintenant_secs();
        let vivante =
            |t: &TentativePkce| maintenant.saturating_sub(t.ouverte_le) < DUREE_TENTATIVE_SECS;
        if !self.tentatives.iter().any(vivante) {
            return Err(match state {
                Some(_) => refus(CODE_STATE_INCONNU, AUCUNE_TENTATIVE),
                None => AUCUNE_TENTATIVE.into(),
            });
        }
        match state {
            None => Ok(self
                .tentatives
                .iter()
                .rposition(vivante)
                .expect("au moins une tentative vivante")),
            Some(state) => self
                .tentatives
                .iter()
                .rposition(|t| t.state == state && vivante(t))
                .ok_or_else(|| refus(CODE_STATE_INCONNU, TENTATIVE_INCONNUE)),
        }
    }

    /// Les adresses de jeton et d'API de Spotify remplacees par `base`, pour
    /// eprouver la connexion par le vrai routeur sans joindre Spotify (#8142).
    /// Reserve aux bancs d'essai : aucun binaire publie ne le compile.
    #[cfg(any(test, feature = "services-factices"))]
    pub fn avec_doublure(mut self, base: &str) -> Self {
        self.token_url = format!("{base}/api/token");
        self.api_base = format!("{base}/v1");
        self
    }

    /// Fil 221 — ce que `GET /system/env` publie : `false` tant que le
    /// service tourne avec `"placeholder"`.
    pub fn client_id_est_configure(&self) -> bool {
        client_id_configure(&self.client_id)
    }

    /// Enregistre un refus de jeton pour que le prochain tick le rafraîchisse.
    ///
    /// Appelé depuis les méthodes HTTP, qui n'ont que `&self` — d'où l'atomique.
    fn note_token_rejected(&self) {
        if !self.token_rejected.swap(true, Ordering::Relaxed) {
            warn!("spotify_token_rejected_refresh_scheduled");
        }
    }

    fn generate_pkce() -> (String, String) {
        // 96 octets du generateur du systeme, en base64url : 128 caracteres,
        // tous dans l'alphabet admis par la RFC 7636. L'ancienne graine
        // (`subsec_nanos() + i`) rendait une suite quasi constante.
        let verifier = base64url_encode(&octets_aleatoires::<96>());

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        let challenge = base64url_encode(&hash);

        (verifier, challenge)
    }

    fn auth_url(&self, challenge: &str, state: &str, redirect_uri: &str) -> String {
        format!(
            "{AUTH_URL}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge_method=S256&code_challenge={challenge}&state={state}",
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(SCOPES),
        )
    }

    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        self.api_get_detaille(path).await.map_err(|e| e.message)
    }

    /// [`Self::api_get`], le statut HTTP lisible ([`ErreurApi`]).
    async fn api_get_detaille(&self, path: &str) -> Result<serde_json::Value, ErreurApi> {
        let token = self
            .access_token
            .as_deref()
            .ok_or_else(|| ErreurApi::sans_statut("not authenticated"))?;
        let reponse = requete_api(&self.client, &self.api_base, token, path, None).await;
        if matches!(&reponse, Err(e) if e.jeton_refuse()) {
            self.note_token_rejected();
        }
        reponse
    }

    /// #8050 — taille d'une page des listes de l'utilisateur : le plafond
    /// que Spotify accepte pour `/me/tracks`, `/me/albums`, `/me/playlists`
    /// et `/me/following`.
    const PAGE_UTILISATEUR: usize = 50;
    /// #8050 — borne de sécurité : 200 pages de 50, soit 10 000 éléments par
    /// liste. Au-delà, la lecture s'arrête et le dit, au lieu de tourner sans
    /// fin sur un service qui annoncerait toujours une suite.
    const PAGES_UTILISATEUR_MAX: usize = 200;

    /// #8050 — tous les éléments d'une liste de l'utilisateur paginée par
    /// `offset` (`/me/tracks`, `/me/albums`, `/me/playlists`).
    ///
    /// On ne lisait que la première page (`?limit=50`) : au-delà de 50
    /// favoris, le reste n'apparaissait jamais dans Tune. Même modèle que la
    /// lecture des favoris TIDAL : le décalage avance du nombre REÇU (#7250,
    /// une page plafonnée sous la limite ne fait sauter personne), la lecture
    /// s'arrête au `total` annoncé ou, sans total, à la première page sans
    /// `next` ; une page qui n'apporte aucun élément neuf l'arrête aussi
    /// (garde de #7729), comme la borne `PAGES_UTILISATEUR_MAX`.
    async fn lire_toutes_les_pages(
        &self,
        chemin: &str,
        cle: fn(&serde_json::Value) -> Option<String>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let separateur = if chemin.contains('?') { '&' } else { '?' };
        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut vus: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut offset = 0usize;
        let mut total: Option<usize> = None;
        let mut termine = false;
        for _ in 0..Self::PAGES_UTILISATEUR_MAX {
            let data = self
                .api_get(&format!(
                    "{chemin}{separateur}limit={}&offset={offset}",
                    Self::PAGE_UTILISATEUR
                ))
                .await?;
            let lot = data["items"].as_array().cloned().unwrap_or_default();
            let recus = lot.len();
            offset += recus;
            let avant = items.len();
            for element in lot {
                let id = cle(&element).unwrap_or_else(|| element.to_string());
                if vus.insert(id) {
                    items.push(element);
                }
            }
            total = data["total"].as_u64().and_then(|t| usize::try_from(t).ok());
            let suite = data["next"].is_string();
            let fini = match total {
                Some(t) => offset >= t,
                None => !suite,
            };
            if fini || recus == 0 {
                termine = true;
                break;
            }
            if items.len() == avant {
                warn!(chemin, offset, "spotify_pagination_sans_progression");
                termine = true;
                break;
            }
        }
        if !termine {
            warn!(
                chemin,
                lus = items.len(),
                "spotify_pagination_borne_atteinte"
            );
        } else if let Some(t) = total.filter(|t| items.len() < *t) {
            warn!(
                chemin,
                lus = items.len(),
                total = t,
                "spotify_pagination_incomplete"
            );
        }
        Ok(items)
    }

    /// #8050 — tous les artistes suivis. `/me/following` pagine par curseur
    /// (`artists.cursors.after`), pas par décalage ; mêmes gardes que
    /// [`Self::lire_toutes_les_pages`].
    async fn lire_tous_les_artistes_suivis(&self) -> Result<Vec<serde_json::Value>, String> {
        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut vus: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut apres: Option<String> = None;
        for _ in 0..Self::PAGES_UTILISATEUR_MAX {
            let curseur = apres
                .as_deref()
                .map(|a| format!("&after={}", urlencoding::encode(a)))
                .unwrap_or_default();
            let data = self
                .api_get(&format!(
                    "/me/following?type=artist&limit={}{curseur}",
                    Self::PAGE_UTILISATEUR
                ))
                .await?;
            let page = &data["artists"];
            let lot = page["items"].as_array().cloned().unwrap_or_default();
            let recus = lot.len();
            let avant = items.len();
            for element in lot {
                let id = element["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| element.to_string());
                if vus.insert(id) {
                    items.push(element);
                }
            }
            let suivant = page["cursors"]["after"].as_str().map(str::to_owned);
            if recus == 0 || !page["next"].is_string() || suivant.is_none() {
                return Ok(items);
            }
            if items.len() == avant {
                warn!(
                    lus = items.len(),
                    "spotify_artistes_suivis_sans_progression"
                );
                return Ok(items);
            }
            apres = suivant;
        }
        warn!(lus = items.len(), "spotify_artistes_suivis_borne_atteinte");
        Ok(items)
    }

    /// #7659 — la limite envoyée à `/v1/search` pour une `limit` demandée :
    /// le plafond accepté par cette application, `0` (« Tous ») compris.
    fn borne_de_recherche(&self, limit: usize) -> usize {
        let plafond = self.plafond_recherche.load(Ordering::Relaxed);
        if limit == 0 {
            plafond
        } else {
            limit.min(plafond)
        }
    }

    /// #7659 — une page brute de `/v1/search`, et la limite réellement servie.
    ///
    /// Un `400 Invalid limit` reçu au-dessus du plafond du mode développement
    /// fait adopter ce plafond, puis la même page est redemandée UNE fois :
    /// la recherche aboutit pour les deux sortes d'applications, sans deviner
    /// laquelle est configurée.
    async fn recherche(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<(serde_json::Value, usize), String> {
        let borne = self.borne_de_recherche(limit);
        match self.page_brute(query, borne, offset).await {
            Err(e) if refus_de_limite(&e) && borne > PLAFOND_RECHERCHE_MODE_DEVELOPPEMENT => {
                self.plafond_recherche
                    .store(PLAFOND_RECHERCHE_MODE_DEVELOPPEMENT, Ordering::Relaxed);
                warn!(
                    refusee = borne,
                    adoptee = PLAFOND_RECHERCHE_MODE_DEVELOPPEMENT,
                    "spotify_search_limit_lowered"
                );
                let borne = self.borne_de_recherche(limit);
                self.page_brute(query, borne, offset)
                    .await
                    .map(|data| (data, borne))
            }
            r => r.map(|data| (data, borne)),
        }
    }

    async fn page_brute(
        &self,
        query: &str,
        borne: usize,
        offset: usize,
    ) -> Result<serde_json::Value, String> {
        self.api_get(&format!(
            "/search?q={}&type=track,album,artist&limit={borne}&offset={offset}",
            urlencoding::encode(query)
        ))
        .await
    }

    fn resultats_de_recherche(data: &serde_json::Value) -> SearchResults {
        SearchResults {
            tracks: data["tracks"]["items"]
                .as_array()
                .map(|i| i.iter().map(Self::map_track).collect())
                .unwrap_or_default(),
            albums: data["albums"]["items"]
                .as_array()
                .map(|i| i.iter().map(Self::map_album).collect())
                .unwrap_or_default(),
            artists: data["artists"]["items"]
                .as_array()
                .map(|i| i.iter().map(Self::map_artist).collect())
                .unwrap_or_default(),
            playlists: vec![],
        }
    }

    /// #6018 — le jeton d'accès courant, que librespot reçoit pour se
    /// connecter au compte (`--access-token`).
    pub fn jeton_d_acces(&self) -> Option<String> {
        self.access_token.clone()
    }
    /// #6933 — la liaison porte-t-elle la portée `streaming`, sans laquelle
    /// Spotify refuse la connexion de librespot (`INVALID_CREDENTIALS`) ?
    /// `None` : on ne le sait pas.
    pub fn porte_la_lecture(&self) -> Option<bool> {
        self.portees
            .as_deref()
            .map(|p| p.split_whitespace().any(|x| x == "streaming"))
    }

    /// #6018 — l'identifiant API Web de l'appareil Connect nommé `nom`.
    pub async fn appareil_par_nom(&self, nom: &str) -> Result<Option<String>, String> {
        let d = self.api_get("/me/player/devices").await?;
        Ok(d["devices"].as_array().and_then(|appareils| {
            appareils
                .iter()
                .find(|a| a["name"].as_str() == Some(nom))
                .and_then(|a| a["id"].as_str())
                .map(Into::into)
        }))
    }

    /// #6018 — demande à l'appareil Connect `nom` (le librespot de Tune) de
    /// jouer `piste` depuis `position_ms`. Un seul titre : la file de Tune
    /// enchaîne. L'appareil met quelques secondes à apparaître après le
    /// démarrage de librespot : il est cherché plusieurs fois.
    pub async fn lancer_sur_l_appareil(
        &self,
        nom: &str,
        piste: &str,
        position_ms: u64,
    ) -> Result<(), String> {
        let token = self
            .access_token
            .clone()
            .ok_or("Spotify n'est pas connecté")?;
        let mut appareil = None;
        let mut derniere_erreur = None;
        for essai in 0..20 {
            if essai > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            // #7659 — `api_get` rend désormais un refus (429, 5xx…) en erreur :
            // un refus passager vaut « appareil pas encore visible », seul le
            // jeton rejeté arrête l'attente.
            match self.appareil_par_nom(nom).await {
                Ok(trouve) => {
                    derniere_erreur = None;
                    appareil = trouve;
                }
                Err(e) if e == "token expired" => return Err(e),
                Err(e) => derniere_erreur = Some(e),
            }
            if appareil.is_some() {
                break;
            }
        }
        let appareil = match (appareil, derniere_erreur) {
            (Some(appareil), _) => appareil,
            (None, Some(e)) => return Err(e),
            (None, None) => return Err(motif_appareil_absent(nom)),
        };
        let resp = self
            .client
            .put(format!(
                "{}/me/player/play?device_id={}",
                self.api_base,
                urlencoding::encode(&appareil)
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "uris": [format!("spotify:track:{piste}")],
                "position_ms": position_ms,
            }))
            .send()
            .await
            .map_err(|e| format!("spotify api: {e}"))?;
        let statut = resp.status().as_u16();
        match statut {
            200 | 202 | 204 => {
                info!(appareil = %appareil, piste, position_ms, "spotify_lecture_lancee");
                Ok(())
            }
            401 => {
                self.note_token_rejected();
                Err("Spotify a refusé le jeton : reconnecter Spotify dans les Réglages.".into())
            }
            _ => {
                let detail = resp.text().await.unwrap_or_default();
                if statut == 403 {
                    Err(motif_refus_403(&detail))
                } else {
                    Err(format!(
                        "Spotify a refusé la lecture (HTTP {statut}) : {detail}"
                    ))
                }
            }
        }
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_str().unwrap_or("").into(),
            title: item["name"].as_str().unwrap_or("").into(),
            artist: item["artists"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["name"].as_str())
                .unwrap_or("")
                .into(),
            album: album["name"].as_str().map(Into::into),
            album_id: album["id"].as_str().map(Into::into),
            duration_ms: item["duration_ms"].as_u64().unwrap_or(0),
            cover_path: album["images"]
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i["url"].as_str())
                .map(Into::into),
            track_number: item["track_number"].as_u64().map(|n| n as u32),
            disc_number: item["disc_number"].as_u64().map(|n| n as u32),
            explicit: item["explicit"].as_bool().unwrap_or(false),
            disponible: None,
            isrc: item["external_ids"]["isrc"].as_str().map(Into::into),
            composer: None,
            artist_id: None,
            quality: Some(StreamQuality {
                codec: "OGG".into(),
                sample_rate: 44100,
                bit_depth: 16,
                bitrate: Some(320),
                channels: 2,
            }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            ai_generated: None,
            explicit: None,
            release_type: None,
            id: item["id"].as_str().unwrap_or("").into(),
            title: item["name"].as_str().unwrap_or("").into(),
            artist: item["artists"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["name"].as_str())
                .unwrap_or("")
                .into(),
            artist_id: item["artists"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["id"].as_str())
                .map(Into::into),
            cover_path: item["images"]
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i["url"].as_str())
                .map(Into::into),
            year: item["release_date"]
                .as_str()
                .and_then(|d| d.get(..4)?.parse().ok()),
            track_count: item["total_tracks"].as_u64().unwrap_or(0) as u32,
            released_at: None,
            quality: None,
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["images"]
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i["url"].as_str())
                .map(Into::into),
            bio: None,
        }
    }

    fn map_playlist(item: &serde_json::Value) -> StreamPlaylist {
        StreamPlaylist {
            id: item["id"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            description: item["description"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map(Into::into),
            cover_path: item["images"]
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i["url"].as_str())
                .map(Into::into),
            track_count: nombre_de_titres(item),
            owner: item["owner"]["display_name"].as_str().map(Into::into),
            editable: None,
            covers: Vec::new(),
        }
    }

    fn map_category_to_genre(item: &serde_json::Value) -> StreamGenre {
        StreamGenre {
            id: item["id"].as_str().unwrap_or("").into(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children: false,
            image_url: item["icons"]
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i["url"].as_str())
                .map(Into::into),
        }
    }

    async fn api_put(&self, path: &str) -> Result<(), String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self
            .client
            .put(format!("{}{path}", self.api_base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("spotify put: {e}"))?;
        if resp.status() == 401 {
            self.note_token_rejected();
            return Err("token expired".into());
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(format!("spotify PUT {path}: {status}"));
        }
        Ok(())
    }

    async fn api_delete(&self, path: &str) -> Result<(), String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self
            .client
            .delete(format!("{}{path}", self.api_base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("spotify delete: {e}"))?;
        if resp.status() == 401 {
            self.note_token_rejected();
            return Err("token expired".into());
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(format!("spotify DELETE {path}: {status}"));
        }
        Ok(())
    }

    /// Février 2026 — un point d'accès que Spotify a retiré aux applications
    /// en mode développement répond 403 (ou 404). `None` : retiré, la
    /// rubrique est vide ; le retrait est mémorisé dans `retire` (une seule
    /// requête, puis plus rien jusqu'au changement d'application). Tout autre
    /// refus reste une erreur.
    async fn lire_ou_retire(
        &self,
        chemin: &str,
        route: &'static str,
        retire: &AtomicBool,
    ) -> Result<Option<serde_json::Value>, TuneError> {
        if retire.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match self.api_get(chemin).await {
            Ok(d) => Ok(Some(d)),
            Err(e) if matches!(statut_du_refus(&e), Some(403 | 404)) => {
                if !retire.swap(true, Ordering::Relaxed) {
                    info!(route, "spotify_point_d_acces_retire");
                }
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn api_post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let token = self.access_token.as_deref().ok_or("not authenticated")?;
        let resp = self
            .client
            .post(format!("{}{path}", self.api_base))
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("spotify post: {e}"))?;
        if resp.status() == 401 {
            self.note_token_rejected();
            return Err("token expired".into());
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("spotify POST {path}: {status} — {text}"));
        }
        resp.json()
            .await
            .map_err(|e| format!("spotify post parse: {e}"))
    }
}

impl SpotifyService {
    /// #8198 — l'adresse de retour envoyee a Spotify : celle du relais quand
    /// l'option est active, sinon `redirect_uri`.
    pub fn adresse_de_retour(&self) -> &str {
        self.relais.as_deref().unwrap_or(&self.redirect_uri)
    }

    /// #8198 — active (`Some(adresse)`) ou coupe (`None`) le retour par le
    /// relais. Les autorisations deja ouvertes gardent leur propre adresse.
    pub fn regler_relais(&mut self, relais: Option<String>) {
        self.relais = relais;
    }

    /// #8198 — le relais est-il actif ?
    pub fn relais_actif(&self) -> bool {
        self.relais.is_some()
    }

    fn oublier_les_tentatives_perimees(&mut self) {
        let maintenant = maintenant_secs();
        self.tentatives
            .retain(|t| maintenant.saturating_sub(t.ouverte_le) < DUREE_TENTATIVE_SECS);
    }

    /// #8198 — les autorisations entamees, a persister : elles survivent
    /// ainsi a un redemarrage du serveur (conteneur mis a jour entre le clic
    /// et le retour). Le verificateur PKCE y figure : la ligne vit dans la
    /// meme base que les jetons, et 15 minutes au plus.
    pub fn tentatives_a_persister(&self) -> serde_json::Value {
        let maintenant = maintenant_secs();
        serde_json::Value::Array(
            self.tentatives
                .iter()
                .filter(|t| maintenant.saturating_sub(t.ouverte_le) < DUREE_TENTATIVE_SECS)
                .map(|t| {
                    serde_json::json!({
                        "state": t.state,
                        "verifier": t.verifier,
                        "redirect_uri": t.redirect_uri,
                        "ouverte_le": t.ouverte_le,
                    })
                })
                .collect(),
        )
    }

    /// #8198 — reprend les autorisations persistees, si ce service n'en a
    /// aucune en memoire (il vient de demarrer). Les perimees sont ignorees.
    /// Rend le nombre de tentatives reprises.
    pub fn reprendre_tentatives(&mut self, persistees: &serde_json::Value) -> usize {
        if !self.tentatives.is_empty() {
            return 0;
        }
        let maintenant = maintenant_secs();
        let Some(liste) = persistees.as_array() else {
            return 0;
        };
        for t in liste {
            let (Some(state), Some(verifier), Some(redirect_uri), Some(ouverte_le)) = (
                t["state"].as_str(),
                t["verifier"].as_str(),
                t["redirect_uri"].as_str(),
                t["ouverte_le"].as_u64(),
            ) else {
                continue;
            };
            if maintenant.saturating_sub(ouverte_le) >= DUREE_TENTATIVE_SECS {
                continue;
            }
            self.tentatives.push(TentativePkce {
                state: state.to_owned(),
                verifier: verifier.to_owned(),
                redirect_uri: redirect_uri.to_owned(),
                ouverte_le,
            });
        }
        if self.tentatives.len() > TENTATIVES_MAX {
            let trop = self.tentatives.len() - TENTATIVES_MAX;
            self.tentatives.drain(..trop);
        }
        self.tentatives.len()
    }

    /// #8198 — les 8 premiers caracteres du Client ID en service, a comparer
    /// avec l'adresse du tableau de bord Spotify ; `None` sans Client ID.
    pub fn debut_du_client_id(&self) -> Option<String> {
        self.client_id_est_configure()
            .then(|| self.client_id.chars().take(8).collect())
    }

    /// #8198 — ce que le verificateur doit lire, copie sous le verrou : les
    /// appels a Spotify ([`InstantaneDiagnostic::rapport`]) se font verrou
    /// relache, pour ne bloquer ni l'ecoute ni une connexion.
    pub fn instantane_diagnostic(&self) -> InstantaneDiagnostic {
        InstantaneDiagnostic {
            client: self.client.clone(),
            client_id: self.client_id.clone(),
            redirect_uri: self.adresse_de_retour().to_owned(),
            relais: self.relais_actif(),
            token_url: self.token_url.clone(),
            api_base: self.api_base.clone(),
            access_token: self.access_token.clone(),
            session_refusee: self.session_refusee,
            username: self.username.clone(),
            derniere_erreur: self.derniere_erreur.clone(),
            token_rejected: Arc::clone(&self.token_rejected),
        }
    }

    /// #8198 — le verificateur, sans verrou a garder : voir
    /// [`Self::instantane_diagnostic`].
    pub async fn diagnostic(&self) -> serde_json::Value {
        self.instantane_diagnostic().rapport().await
    }

    /// L'authentification proprement dite ; [`StreamingService::authenticate`]
    /// l'enveloppe pour noter le dernier refus (#8198).
    async fn authentifier(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        // Fil 221 — sans Client ID, l'URL d'autorisation portait
        // `client_id=placeholder` et l'echange d'un code finissait en
        // `invalid_client` : refuser tout de suite, en disant ou le poser.
        if !self.client_id_est_configure() {
            return Err(refus(
                CODE_CLIENT_ID_ABSENT,
                format!(
                    "{CLIENT_ID_ABSENT} : enregistrez celui de votre application Spotify \
                     (developer.spotify.com, tableau de bord) dans Reglages > Acces et jetons > \
                     Services de streaming, ou posez TUNE_SPOTIFY_CLIENT_ID"
                ),
            ));
        }
        // #2680 — `callback_url` : l'adresse de rappel collee depuis un autre
        // poste, ou le rappel `127.0.0.1` ne peut pas aboutir.
        let rappel = match credentials.get("callback_url").and_then(|v| v.as_str()) {
            Some(colle) => Some(rappel_depuis_colle(colle)?),
            None => credentials
                .get("code")
                .and_then(|v| v.as_str())
                .map(|code| RappelSpotify {
                    code: code.to_owned(),
                    state: credentials
                        .get("state")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                }),
        };
        // Fil 221 — `GET /streaming/spotify/status`, que l'ecran sonde toutes
        // les 3 s pendant l'attente, appelle `authenticate({"poll": true})`.
        // Sans ce garde, chaque sondage relancait une autorisation et
        // ECRASAIT le verificateur : le code colle ensuite ne correspondait
        // plus (`invalid_grant code_verifier was incorrect`). Spotify n'a pas
        // de flux a sonder : un sondage rend l'etat, rien de plus.
        if rappel.is_none() && credentials.get("poll").is_some() {
            return Ok(self.auth_status().await);
        }
        if let Some(RappelSpotify { code, state }) = rappel {
            let code = code.as_str();
            let indice = self.tentative_pour(state.as_deref())?;
            let verifier = self.tentatives[indice].verifier.clone();
            let redirect_uri = self.tentatives[indice].redirect_uri.clone();
            let resp = self
                .client
                .post(&self.token_url)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &redirect_uri),
                    ("client_id", &self.client_id),
                    ("code_verifier", &verifier),
                ])
                .send()
                .await
                .map_err(|e| format!("token: {e}"))?;

            let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
            if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                // #2680 — nommer l'URI envoyee : c'est elle qui doit figurer
                // A L'IDENTIQUE dans le tableau de bord Spotify.
                let detail = data
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // Fil 221 — un code ne sert qu'une fois, mais le verificateur
                // reste bon pour le MEME lien d'autorisation : la tentative
                // est gardee, rouvrir le lien donne un nouveau code.
                let conseil = if err == "invalid_grant" {
                    " — un code ne sert qu'une fois : rouvrez le lien d'autorisation, \
                     puis collez la NOUVELLE adresse de retour"
                } else {
                    ""
                };
                let message = format!(
                    "spotify: {err} {detail} (redirect_uri envoyee : {redirect_uri}){conseil}"
                );
                // #8198 — un code stable pour les trois refus courants ;
                // les autres gardent leur texte seul.
                return Err(match code_du_refus_d_echange(err, detail) {
                    Some(code) => refus(code, message),
                    None => message.into(),
                });
            }
            self.tentatives.clear();

            // #8198 — la session en place, rendue telle quelle si Spotify
            // refuse ce compte (`/me` en 403) : un essai rate ne doit pas
            // deconnecter un utilisateur dont la liaison marche.
            let session_valide = self.access_token.is_some()
                && !self.session_refusee
                && !self.token_rejected.load(Ordering::Relaxed);
            let session_precedente = (
                self.access_token.take(),
                self.refresh_token.take(),
                self.portees.take(),
                self.token_expires.take(),
            );

            self.access_token = data["access_token"].as_str().map(Into::into);
            self.refresh_token = data["refresh_token"].as_str().map(Into::into);
            self.portees = data["scope"].as_str().map(Into::into);
            let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
            self.token_expires =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(expires_in));
            self.token_rejected.store(false, Ordering::Relaxed);
            // #8198 — en mode developpement, Spotify delivre le jeton a un
            // compte absent de « User Management » (ou dont l'application a
            // un proprietaire sans Premium), puis refuse chaque appel en 403.
            // Le constater ICI, et ne pas garder une liaison inutilisable.
            let me = match self.api_get_detaille("/me").await {
                Ok(me) => Some(me),
                Err(e) if e.compte_non_autorise() => {
                    warn!(motif = %e.message, "spotify_compte_non_autorise");
                    if session_valide {
                        (
                            self.access_token,
                            self.refresh_token,
                            self.portees,
                            self.token_expires,
                        ) = session_precedente;
                    } else {
                        self.access_token = None;
                        self.refresh_token = None;
                        self.portees = None;
                        self.token_expires = None;
                    }
                    return Err(refus(CODE_COMPTE_NON_AUTORISE, COMPTE_NON_AUTORISE));
                }
                Err(_) => None,
            };
            self.username = me
                .as_ref()
                .and_then(|v| v["display_name"].as_str().map(Into::into));
            self.user_id = me.as_ref().and_then(|v| v["id"].as_str().map(Into::into));
            info!(username = ?self.username, user_id = ?self.user_id, "spotify_authenticated");
            return Ok(self.auth_status().await);
        }

        let (verifier, challenge) = Self::generate_pkce();
        let nonce = base64url_encode(&octets_aleatoires::<16>());
        // #8198 — relais actif : l'origine de la page Tune voyage dans le
        // `state` (`<nonce>.<origine en base64url>`), pour que le relais
        // sache ou renvoyer le navigateur. Sans origine, le nonce seul : le
        // relais n'offre alors que l'adresse a copier.
        let redirect_uri = self.adresse_de_retour().to_owned();
        let state = match (
            &self.relais,
            credentials
                .get("origine")
                .and_then(|v| v.as_str())
                .and_then(normaliser_origine),
        ) {
            (Some(_), Some(origine)) => {
                format!("{nonce}.{}", base64url_encode(origine.as_bytes()))
            }
            _ => nonce,
        };
        let url = self.auth_url(&challenge, &state, &redirect_uri);
        self.oublier_les_tentatives_perimees();
        self.tentatives.push(TentativePkce {
            state,
            verifier,
            redirect_uri,
            ouverte_le: maintenant_secs(),
        });
        if self.tentatives.len() > TENTATIVES_MAX {
            let trop = self.tentatives.len() - TENTATIVES_MAX;
            self.tentatives.drain(..trop);
        }
        Ok(AuthStatus {
            authenticated: false,
            verification_url: Some(url),
            ..Default::default()
        })
    }
}

/// #8198 — copie de l'etat du service pour le verificateur de configuration,
/// prise sous le verrou puis lue sans lui.
pub struct InstantaneDiagnostic {
    client: Client,
    client_id: String,
    /// L'adresse de retour envoyee a Spotify (celle du relais s'il est actif).
    redirect_uri: String,
    relais: bool,
    token_url: String,
    api_base: String,
    access_token: Option<String>,
    session_refusee: bool,
    username: Option<String>,
    derniere_erreur: Option<DerniereErreur>,
    token_rejected: Arc<AtomicBool>,
}

impl InstantaneDiagnostic {
    /// #8198 — Spotify accepte-t-il ce Client ID ? Un echange de code factice
    /// (`/api/token`) repond `invalid_client` (« Failed to get client ») quand
    /// il le refuse (`refuse`), `invalid_grant` sinon (`accepte`). Aucun
    /// compte n'est en jeu. `non_verifie` si Spotify ne repond pas. Un refus
    /// ne prouve pas que l'identifiant est faux : application supprimee ou
    /// desactivee, proprietaire sans Premium (mode developpement) sont des
    /// causes possibles (voir `rapport`). Delai : [`DELAI_SONDE_DIAGNOSTIC`].
    async fn sonder_client_id(&self) -> &'static str {
        let reponse = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", "tune-diagnostic-code-factice"),
                ("redirect_uri", &self.redirect_uri),
                ("client_id", &self.client_id),
                (
                    "code_verifier",
                    "tune-diagnostic-verificateur-factice-0123456789",
                ),
            ])
            .timeout(DELAI_SONDE_DIAGNOSTIC)
            .send()
            .await;
        let Ok(reponse) = reponse else {
            return "non_verifie";
        };
        match reponse.json::<serde_json::Value>().await {
            Ok(v) if v["error"] == "invalid_client" => "refuse",
            Ok(_) => "accepte",
            Err(_) => "non_verifie",
        }
    }

    /// #8198 — le verificateur de configuration (`GET
    /// /streaming/spotify/diagnostic`) : ce qui ne va pas, dit precisement.
    ///
    /// Tune ne peut pas lire le tableau de bord Spotify. Il dit donc ce qu'il
    /// sait : son Client ID, l'adresse de retour qu'il envoie (a declarer a
    /// l'identique), l'etat de la liaison, et, si elle existe, ce que
    /// repond `GET /me` (403 : compte absent de User Management ou
    /// proprietaire sans Premium). `premium` vient de `/me.product`, que le
    /// mode developpement ne renvoie plus depuis fevrier 2026 : `inconnu`.
    pub async fn rapport(self) -> serde_json::Value {
        let mut a_corriger: Vec<String> = Vec::new();

        let client_id = if !client_id_configure(&self.client_id) {
            a_corriger.push("client_id_absent".into());
            "absent"
        } else if !ressemble_a_un_client_id(&self.client_id) {
            a_corriger.push("client_id_forme_invalide".into());
            "forme_invalide"
        } else {
            "ok"
        };
        // Retour d'un testeur (09/11) : le Client ID enregistre n'etait pas
        // celui de son application. Spotify le dit sans qu'aucun compte se
        // connecte : un echange de code factice repond `invalid_client` pour
        // un identifiant qu'il ne connait pas. Les 8 premiers caracteres se
        // comparent a l'adresse du tableau de bord (un Client ID n'est pas un
        // secret).
        let accueil_spotify = if client_id == "ok" {
            let accueil = self.sonder_client_id().await;
            if accueil == "refuse" {
                a_corriger.push("client_id_refuse_par_spotify".into());
            }
            accueil
        } else {
            "non_verifie"
        };
        // Retour d'un testeur (09/11) : `INVALID_CLIENT: Invalid client` avec
        // un Client ID pourtant correct (verifie dans Settings › Basic
        // Information). Les causes probables, a verifier dans l'ordre.
        let refus_du_client = accueil_spotify == "refuse"
            || self
                .derniere_erreur
                .as_ref()
                .is_some_and(|d| d.code == CODE_CLIENT_ID_REFUSE);
        let causes_possibles: Vec<&str> = if refus_du_client {
            vec![
                "connexion_lancee_avant_le_client_id",
                "proprietaire_sans_premium",
                "application_supprimee_ou_desactivee",
            ]
        } else {
            Vec::new()
        };
        let debut = client_id_configure(&self.client_id)
            .then(|| self.client_id.chars().take(8).collect::<String>());

        let redirect = match refus_redirection(&self.redirect_uri) {
            None => "ok",
            Some(RefusRedirection::Localhost) => {
                a_corriger.push("redirect_localhost".into());
                "localhost"
            }
            Some(RefusRedirection::HttpHorsBouclage) => {
                a_corriger.push("redirect_http_hors_bouclage".into());
                "http_hors_bouclage"
            }
        };

        let connexion = if self.access_token.is_some() {
            "connecte"
        } else if self.session_refusee {
            a_corriger.push("session_expiree".into());
            "session_expiree"
        } else {
            a_corriger.push("non_connecte".into());
            "non_connecte"
        };

        let (acces, detail, premium) = if self.access_token.is_none() {
            ("non_verifie", None, "inconnu")
        } else {
            let jeton = self.access_token.as_deref().unwrap_or_default();
            let me = requete_api(
                &self.client,
                &self.api_base,
                jeton,
                "/me",
                Some(DELAI_SONDE_DIAGNOSTIC),
            )
            .await;
            if matches!(&me, Err(e) if e.jeton_refuse()) {
                // Le prochain rafraichissement renouvellera le jeton.
                self.token_rejected.store(true, Ordering::Relaxed);
            }
            match me {
                Ok(me) => {
                    let premium = match me["product"].as_str() {
                        Some("premium") => "oui",
                        Some(_) => {
                            a_corriger.push("premium_absent".into());
                            "non"
                        }
                        None => "inconnu",
                    };
                    ("ok", None, premium)
                }
                Err(e) if e.compte_non_autorise() => {
                    a_corriger.push("compte_non_autorise".into());
                    ("compte_non_autorise", Some(e.message), "inconnu")
                }
                Err(e) if e.jeton_refuse() => {
                    a_corriger.push("jeton_refuse".into());
                    ("jeton_refuse", Some(e.message), "inconnu")
                }
                Err(e) => ("erreur", Some(e.message), "inconnu"),
            }
        };

        let derniere = self.derniere_erreur.as_ref().map(|d| {
            a_corriger.push(d.code.to_owned());
            serde_json::json!({
                "code": d.code,
                "quand": chrono::DateTime::<chrono::Utc>::from(d.quand)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            })
        });

        serde_json::json!({
            "service": "spotify",
            "client_id": {
                "etat": client_id,
                "spotify": accueil_spotify,
                "debut": debut,
                "causes_possibles": causes_possibles,
            },
            "redirect_uri": {
                "valeur": self.redirect_uri,
                "etat": redirect,
                "relais": self.relais,
            },
            "connexion": { "etat": connexion, "compte": self.username },
            "acces_api": { "etat": acces, "detail": detail },
            "premium": { "etat": premium },
            "derniere_erreur": derniere,
            "a_corriger": a_corriger,
        })
    }
}

#[async_trait::async_trait]
impl StreamingService for SpotifyService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "spotify"
    }
    fn enabled(&self) -> bool {
        self.enabled_override
            .unwrap_or(self.client_id != DEFAULT_CLIENT_ID)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        let issue = self.authentifier(credentials).await;
        // #8198 — le verificateur rapporte le dernier refus code ; une
        // connexion reussie l'efface.
        match &issue {
            Err(TuneError::Refused { code, .. }) => {
                self.derniere_erreur = Some(DerniereErreur {
                    code,
                    quand: std::time::SystemTime::now(),
                });
            }
            Ok(status) if status.authenticated => self.derniere_erreur = None,
            _ => {}
        }
        issue
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.access_token.is_some(),
            username: self.username.clone(),
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        self.access_token = None;
        self.refresh_token = None;
        self.username = None;
        self.user_id = None;
        self.token_expires = None;
        self.portees = None;
        self.token_rejected.store(false, Ordering::Relaxed);
        self.session_refusee = false;
        self.titres_phares_retires.store(false, Ordering::Relaxed);
        self.selection_retiree.store(false, Ordering::Relaxed);
        self.nouveautes_retirees.store(false, Ordering::Relaxed);
        self.genres_retires.store(false, Ordering::Relaxed);
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let (data, _) = self.recherche(query, limit, 0).await?;
        Ok(Self::resultats_de_recherche(&data))
    }

    /// #7659 — la limite servie est celle de [`Self::borne_de_recherche`] :
    /// le curseur de la route avance de ce que Spotify a vraiment rendu.
    fn limite_de_page_recherche(&self, limit: usize) -> usize {
        self.borne_de_recherche(limit)
    }

    /// #7659 — une PAGE de la recherche Spotify : `/v1/search` sait paginer
    /// (`offset`), et chaque catégorie annonce son `total` et, tant qu'il en
    /// reste, `next`. Avec un plafond de 10 par page, c'est « Charger plus »
    /// qui donne la suite.
    async fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, TuneError> {
        let (data, _) = self.recherche(query, limit, offset).await?;
        let total = |cat: &str| data[cat]["total"].as_u64().unwrap_or(0) as usize;
        let has_more = ["tracks", "albums", "artists"]
            .iter()
            .any(|cat| data[*cat]["next"].is_string());
        Ok(SearchPage {
            results: Self::resultats_de_recherche(&data),
            offset,
            totals: SearchTotals {
                tracks: total("tracks"),
                albums: total("albums"),
                artists: total("artists"),
                playlists: 0,
            },
            has_more,
            truncated: false,
        })
    }

    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        let d = self.api_get(&format!("/tracks/{id}")).await?;
        Ok(Self::map_track(&d))
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("Spotify requires Connect/librespot for streaming".into())
    }
    async fn get_album(&self, id: &str) -> Result<StreamAlbum, TuneError> {
        let d = self.api_get(&format!("/albums/{id}")).await?;
        Ok(Self::map_album(&d))
    }
    async fn get_album_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let d = self
            .api_get(&format!("/albums/{id}/tracks?limit=50"))
            .await?;
        Ok(d["items"]
            .as_array()
            .map(|i| i.iter().map(Self::map_track).collect())
            .unwrap_or_default())
    }
    async fn get_artist(&self, id: &str) -> Result<StreamArtist, TuneError> {
        let d = self.api_get(&format!("/artists/{id}")).await?;
        Ok(Self::map_artist(&d))
    }
    async fn get_playlist(&self, id: &str) -> Result<StreamPlaylist, TuneError> {
        let d = self.api_get(&format!("/playlists/{id}")).await?;
        Ok(StreamPlaylist {
            id: d["id"].as_str().unwrap_or("").into(),
            name: d["name"].as_str().unwrap_or("").into(),
            description: d["description"].as_str().map(Into::into),
            cover_path: d["images"]
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i["url"].as_str())
                .map(Into::into),
            track_count: nombre_de_titres(&d),
            owner: d["owner"]["display_name"].as_str().map(Into::into),
            editable: None,
            covers: Vec::new(),
        })
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        // #8193 — `/playlists/{id}/tracks` est retiré par Spotify depuis la
        // migration du 9 mars 2026 (applications en mode développement :
        // 403) ; la route est `/items`, et chaque entrée porte `item` au lieu
        // de `track`. Les playlists s'ouvraient vides.
        let d = self
            .api_get(&format!("/playlists/{id}/items?limit=100"))
            .await?;
        Ok(d["items"]
            .as_array()
            .map(|i| {
                i.iter()
                    .filter_map(element_de_playlist)
                    .map(Self::map_track)
                    .collect()
            })
            .unwrap_or_default())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        // #8050 — toutes les pages, pas seulement les 50 premières.
        let items = self
            .lire_toutes_les_pages("/me/playlists", |p| p["id"].as_str().map(str::to_owned))
            .await?;
        Ok(items.iter().map(Self::map_playlist).collect())
    }

    async fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<String, TuneError> {
        self.user_id
            .as_deref()
            .ok_or("spotify: not authenticated (no user_id)")?;
        let body = serde_json::json!({
            "name": name,
            "description": description.unwrap_or("Created by Tune"),
            "public": false
        });
        let resp = self
            // #8193 — `/users/{id}/playlists` est retiré (février 2026).
            .api_post("/me/playlists", body)
            .await?;
        resp["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "spotify: no playlist id in response".into())
    }

    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        let uris: Vec<String> = track_ids
            .iter()
            .map(|id| {
                if id.starts_with("spotify:track:") {
                    id.clone()
                } else {
                    format!("spotify:track:{id}")
                }
            })
            .collect();
        let mut added = 0;
        for chunk in uris.chunks(100) {
            let body = serde_json::json!({ "uris": chunk });
            // #8193 — `/tracks` retiré au profit de `/items` (février 2026).
            self.api_post(&format!("/playlists/{playlist_id}/items"), body)
                .await?;
            added += chunk.len();
        }
        Ok(added)
    }

    fn supports_write(&self) -> bool {
        self.access_token.is_some() && self.user_id.is_some()
    }

    /// #8479 — la discographie, par pages de [`PAGE_ALBUMS_ARTISTE`].
    ///
    /// On demandait `limit=50` en une fois. Depuis les changements de l'API
    /// Web de février 2026, `GET /artists/{id}/albums` n'accepte plus qu'un
    /// `limit` de 0 à 10 (5 par défaut) : à 50, Spotify refuse la requête
    /// (`400 Invalid limit`) et la fiche artiste perdait TOUS ses albums
    /// Spotify, sans un mot (Ronin22, ASR #169, rc11). On lit donc page par
    /// page, par `offset`, jusqu'au `total` annoncé ou à la première page sans
    /// `next`, dans la limite de [`PAGES_ALBUMS_ARTISTE_MAX`].
    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let mut albums: Vec<StreamAlbum> = Vec::new();
        let mut vus: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut offset = 0usize;
        for _ in 0..PAGES_ALBUMS_ARTISTE_MAX {
            let data = self
                .api_get(&format!(
                    "/artists/{artist_id}/albums?include_groups=album,single&limit={PAGE_ALBUMS_ARTISTE}&offset={offset}"
                ))
                .await?;
            let lot = data["items"].as_array().cloned().unwrap_or_default();
            let recus = lot.len();
            offset += recus;
            for item in &lot {
                let album = Self::map_album(item);
                if album.id.is_empty() || vus.insert(album.id.clone()) {
                    albums.push(album);
                }
            }
            let fini = match data["total"].as_u64() {
                Some(t) => offset as u64 >= t,
                None => !data["next"].is_string(),
            };
            if fini || recus == 0 {
                break;
            }
        }
        Ok(albums)
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        // #8485 — retiré par Spotify à cette application : section vide, sans
        // nouvel appel. La route complète ensuite la liste par ses autres
        // sources (écoutes de ce serveur).
        if self.titres_phares_retires.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let data = match self
            .api_get(&format!("/artists/{artist_id}/top-tracks"))
            .await
        {
            Ok(d) => d,
            Err(e) if statut_du_refus(&e) == Some(403) => {
                if !self.titres_phares_retires.swap(true, Ordering::Relaxed) {
                    info!(
                        route = "/artists/{id}/top-tracks",
                        "spotify_point_d_acces_retire"
                    );
                }
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        Ok(data["tracks"]
            .as_array()
            .map(|i| i.iter().map(Self::map_track).collect())
            .unwrap_or_default())
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        // Février 2026 — retiré par Spotify aux applications en mode
        // développement : rubrique vide, sans « HTTP 502 » brut à l'écran.
        let Some(data) = self
            .lire_ou_retire(
                "/browse/new-releases?limit=50",
                "/browse/new-releases",
                &self.nouveautes_retirees,
            )
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(data["albums"]["items"]
            .as_array()
            .map(|i| i.iter().map(Self::map_album).collect())
            .unwrap_or_default())
    }

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        // #8399 — fermé par Spotify à cette application : rubrique vide, sans
        // nouvel appel (un 403 était rendu en « HTTP 502 » brut à l'écran).
        if self.selection_retiree.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let data = match self.api_get("/browse/featured-playlists?limit=50").await {
            Ok(d) => d,
            Err(e) if statut_du_refus(&e) == Some(403) => {
                if !self.selection_retiree.swap(true, Ordering::Relaxed) {
                    info!(
                        route = "/browse/featured-playlists",
                        "spotify_point_d_acces_retire"
                    );
                }
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        Ok(data["playlists"]["items"]
            .as_array()
            .map(|i| i.iter().map(Self::map_playlist).collect())
            .unwrap_or_default())
    }

    async fn get_genres(&self, _parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        // Février 2026 — retiré par Spotify, comme les nouveautés.
        let Some(data) = self
            .lire_ou_retire(
                "/browse/categories?limit=50",
                "/browse/categories",
                &self.genres_retires,
            )
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(data["categories"]["items"]
            .as_array()
            .map(|i| i.iter().map(Self::map_category_to_genre).collect())
            .unwrap_or_default())
    }

    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let Some(data) = self
            .lire_ou_retire(
                &format!("/browse/categories/{genre_id}/playlists?limit={limit}"),
                "/browse/categories/{id}/playlists",
                &self.genres_retires,
            )
            .await?
        else {
            return Ok(Vec::new());
        };
        let playlists: Vec<String> = data["playlists"]["items"]
            .as_array()
            .map(|i| {
                i.iter()
                    .filter_map(|p| p["id"].as_str().map(Into::into))
                    .collect()
            })
            .unwrap_or_default();
        let mut albums = Vec::new();
        for pid in playlists.iter().take(3) {
            if let Ok(tracks) = self.get_playlist_tracks(pid).await {
                for t in tracks {
                    if let (Some(album_id), Some(album_title)) = (&t.album_id, &t.album)
                        && !albums.iter().any(|a: &StreamAlbum| a.id == *album_id)
                    {
                        albums.push(StreamAlbum {
                            ai_generated: None,
                            explicit: None,
                            release_type: None,
                            id: album_id.clone(),
                            title: album_title.clone(),
                            artist: t.artist.clone(),
                            artist_id: None,
                            cover_path: t.cover_path.clone(),
                            year: None,
                            track_count: 0,
                            released_at: None,
                            quality: None,
                        });
                        if albums.len() >= limit {
                            break;
                        }
                    }
                }
                if albums.len() >= limit {
                    break;
                }
            }
        }
        Ok(albums)
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        // #8050 — toutes les pages, pas seulement les 50 premières.
        let items = self
            .lire_toutes_les_pages("/me/tracks", |e| {
                e["track"]["id"].as_str().map(str::to_owned)
            })
            .await?;
        Ok(items
            .iter()
            .filter_map(|item| item.get("track").map(Self::map_track))
            .collect())
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let path = match fav_type {
            "tracks" => format!("/me/tracks?ids={item_id}"),
            "albums" => format!("/me/albums?ids={item_id}"),
            "artists" => format!("/me/following?type=artist&ids={item_id}"),
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_put(&path).await?;
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let path = match fav_type {
            "tracks" => format!("/me/tracks?ids={item_id}"),
            "albums" => format!("/me/albums?ids={item_id}"),
            "artists" => format!("/me/following?type=artist&ids={item_id}"),
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_delete(&path).await?;
        Ok(())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        // #8050 — toutes les pages, pas seulement les 50 premières.
        let items = self
            .lire_toutes_les_pages("/me/albums", |e| {
                e["album"]["id"].as_str().map(str::to_owned)
            })
            .await?;
        Ok(items
            .iter()
            .filter_map(|item| item.get("album").map(Self::map_album))
            .collect())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        // #8050 — tous les curseurs, pas seulement les 50 premiers.
        let items = self.lire_tous_les_artistes_suivis().await?;
        Ok(items.iter().map(Self::map_artist).collect())
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        let needs_refresh = spotify_needs_refresh(
            self.token_expires,
            self.access_token.is_some(),
            self.refresh_token.is_some(),
            self.token_rejected.load(Ordering::Relaxed),
        );

        if !needs_refresh {
            return Ok(false);
        }

        let refresh_token = self
            .refresh_token
            .as_ref()
            .ok_or("no refresh token")?
            .clone();

        let resp = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("refresh: {e}"))?;

        // #8051 — le refus se lit : statut et motif. Un refus définitif
        // efface la session au lieu de la laisser « Connectée » avec un jeton
        // que Spotify n'accepte plus ; un refus passager (429, 5xx) la garde
        // pour le prochain tic.
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let corps = resp.text().await.unwrap_or_default();
            let code: serde_json::Value = serde_json::from_str(&corps).unwrap_or_default();
            let code = code["error"].as_str().unwrap_or_default().to_owned();
            let message = motif_du_refus(&corps);
            if refus_de_renouvellement_definitif(status, &code) {
                warn!(status, code = %code, message = %message, "spotify_session_refusee");
                self.access_token = None;
                self.refresh_token = None;
                self.token_expires = None;
                self.token_rejected.store(false, Ordering::Relaxed);
                self.session_refusee = true;
                return Ok(true);
            }
            warn!(status, code = %code, message = %message, "spotify_token_refresh_refused");
            return Err(format!("spotify refresh: HTTP {status} — {message}").into());
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        if let Some(at) = data["access_token"].as_str() {
            self.access_token = Some(at.into());
            if let Some(rt) = data["refresh_token"].as_str() {
                self.refresh_token = Some(rt.into());
            }
            if let Some(portees) = data["scope"].as_str() {
                self.portees = Some(portees.into());
            }
            let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
            self.token_expires =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(expires_in));
            self.token_rejected.store(false, Ordering::Relaxed);
            info!("spotify_token_refreshed");
            Ok(true)
        } else {
            Err(format!("spotify refresh: HTTP {status} sans access_token").into())
        }
    }

    fn session_expired(&self) -> bool {
        self.session_refusee && self.access_token.is_none()
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        self.access_token.as_ref().map(|t| {
            serde_json::json!({
                "access_token": t,
                "refresh_token": self.refresh_token,
                "username": self.username,
                "user_id": self.user_id,
                "scope": self.portees,
            })
        })
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(at) = tokens["access_token"].as_str() {
            self.access_token = Some(at.into());
            self.refresh_token = tokens["refresh_token"].as_str().map(Into::into);
            self.username = tokens["username"].as_str().map(Into::into);
            self.user_id = tokens["user_id"].as_str().map(Into::into);
            self.portees = tokens["scope"].as_str().map(Into::into);
            // L'échéance reste `None` : la ligne persistée ne dit ni quand le
            // jeton a été émis ni combien de temps il vit (`save_tokens`
            // n'écrit que les jetons et l'identité). Inventer `now + 3600`
            // faisait croire au rafraîchisseur qu'un jeton peut-être mort
            // depuis des jours était valide, et le neutralisait 55 minutes.
            // `spotify_needs_refresh` traite `None` comme « rafraîchir » —
            // même convention que Tidal et YouTube.
            self.token_expires = None;
            self.token_rejected.store(false, Ordering::Relaxed);
            self.session_refusee = false;
            true
        } else {
            false
        }
    }

    async fn post_restore(&mut self) {
        if let Ok(me) = self.api_get("/me").await {
            self.username = me["display_name"].as_str().map(Into::into);
            self.user_id = me["id"].as_str().map(Into::into);
        }
    }
}

/// Décide si le jeton d'accès doit être renouvelé.
///
/// `expires == None` signifie « échéance inconnue », l'état d'un jeton relu
/// depuis la base au démarrage : `save_tokens` n'écrit ni la date d'émission ni
/// la durée de vie. Convention alignée sur Tidal (`tidal.rs`,
/// `refresh_if_needed`) et YouTube (`youtube.rs`, `token_needs_refresh`) : une
/// échéance inconnue **déclenche** un rafraîchissement, elle ne vaut pas
/// « valide ».
///
/// Spotify faisait exactement l'inverse : `None` ⇒ ne jamais rafraîchir, ce qui
/// forçait `restore_tokens` à inventer `now + 3600`. Le rafraîchisseur
/// périodique était alors neutralisé 55 minutes après chaque redémarrage, et
/// aucun 401 ne rattrapait — d'où le `rejected`, posé par les méthodes HTTP.
fn spotify_needs_refresh(
    expires: Option<std::time::Instant>,
    has_access: bool,
    has_refresh: bool,
    rejected: bool,
) -> bool {
    if rejected {
        return has_access && has_refresh;
    }
    match expires {
        Some(exp) => exp
            .checked_duration_since(std::time::Instant::now())
            .map(|d| d.as_secs() < 300)
            .unwrap_or(true),
        None => has_access && has_refresh,
    }
}

/// #7659 — le message d'un refus de l'API Web (`error.message`), ou le corps
/// brut s'il n'en porte pas.
/// #7659 — l'erreur d'[`SpotifyService::api_get`] est-elle le refus de la
/// limite de recherche (`400 Invalid limit`) ?
fn refus_de_limite(erreur: &str) -> bool {
    erreur.contains("HTTP 400") && erreur.to_lowercase().contains("invalid limit")
}

/// #6933 — l'appareil librespot n'apparaît pas dans `/me/player/devices`.
/// La raison est celle que librespot a écrite (voir
/// `spotify_lecture::motif_echec_librespot`) ; ce motif-ci ne présume pas
/// d'un problème de liaison.
pub fn motif_appareil_absent(nom: &str) -> String {
    format!(
        "L'appareil Spotify Connect « {nom} » n'apparaît pas : librespot ne s'est pas connecté \
         au compte (Spotify Premium est exigé)."
    )
}

/// #8399 — le statut HTTP d'un refus rendu par `api_get`
/// (« spotify GET /route: HTTP 403 — motif »), s'il y en a un.
fn statut_du_refus(erreur: &str) -> Option<u16> {
    let (_, apres) = erreur.split_once(": HTTP ")?;
    apres
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
}

/// #6933 — un 403 de `/me/player/play` n'est PAS un problème de connexion
/// dans le cas général : compte sans Premium, titre indisponible dans le pays,
/// restriction de l'appareil. Seule une portée manquante se règle en
/// reconnectant le compte.
fn motif_refus_403(detail: &str) -> String {
    let corps: serde_json::Value = serde_json::from_str(detail).unwrap_or_default();
    let raison = corps["error"]["reason"].as_str().unwrap_or("");
    let message = corps["error"]["message"].as_str().unwrap_or(detail);
    let bas = message.to_lowercase();
    if raison == "PREMIUM_REQUIRED" || bas.contains("premium") {
        format!("Spotify refuse la lecture : un compte Spotify Premium est exigé. ({message})")
    } else if bas.contains("scope") {
        format!(
            "Spotify refuse la lecture : la liaison du compte ne porte pas les droits de \
             lecture ; reconnecter Spotify une fois dans les Réglages. ({message})"
        )
    } else {
        format!(
            "Spotify refuse la lecture de ce titre (HTTP 403{}) : {message}",
            if raison.is_empty() {
                String::new()
            } else {
                format!(", {raison}")
            }
        )
    }
}

/// #8051 — le refus de renouvellement est-il définitif ? `invalid_grant`
/// (jeton de renouvellement révoqué ou expiré), ou un 400/401 : ni l'un ni
/// l'autre ne se règle en réessayant. Un 429 ou un 5xx est passager.
fn refus_de_renouvellement_definitif(status: u16, code: &str) -> bool {
    code == "invalid_grant" || status == 400 || status == 401
}

/// #8193 — le nombre de titres d'une playlist. Depuis février 2026, Spotify
/// le rend sous `items.total` (anciennement `tracks.total`) ; les deux sont
/// lus, le nouveau d'abord.
fn nombre_de_titres(playlist: &serde_json::Value) -> u32 {
    playlist["items"]["total"]
        .as_u64()
        .or_else(|| playlist["tracks"]["total"].as_u64())
        .unwrap_or(0) as u32
}

/// #8193 — le titre d'une entrée de playlist : `item` depuis février 2026,
/// `track` avant. Une entrée sans objet (titre retiré du catalogue) est sautée.
fn element_de_playlist(entree: &serde_json::Value) -> Option<&serde_json::Value> {
    entree
        .get("item")
        .filter(|v| v.is_object())
        .or_else(|| entree.get("track").filter(|v| v.is_object()))
}

/// #6825 — le motif d'un refus de l'API Web, borné : `error.message` du
/// corps JSON de Spotify (`{"error": {"status", "message"}}`), sinon le début
/// du texte brut. Jamais plus de 200 caractères dans le journal.
fn motif_du_refus(corps: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(corps).unwrap_or_default();
    let message = v["error"]["message"]
        .as_str()
        .or_else(|| v["error_description"].as_str())
        .unwrap_or(corps)
        .trim();
    message.chars().take(200).collect()
}
/// Base64url sans remplissage (RFC 4648 §5) : verificateur, `state`, origine
/// du relais (#8198).
pub fn base64url_encode(data: &[u8]) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        buf = (buf << 8) | byte as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            output.push(table[((buf >> bits) & 0x3F) as usize] as char);
        }
    }
    if bits > 0 {
        buf <<= 6 - bits;
        output.push(table[(buf & 0x3F) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spotify_default_redirect_uses_the_tune_port_and_an_explicit_loopback() {
        assert_eq!(
            resolve_redirect_uri(None, None, None, DEFAULT_API_PORT),
            "http://127.0.0.1:8888/api/v1/streaming/spotify/callback"
        );
        let defaut = default_redirect_uri(DEFAULT_API_PORT);
        assert!(!defaut.contains("localhost"));
        assert!(!defaut.contains(":8085/"));
    }

    /// #2680 — le defaut SUIT le port ecoute.
    ///
    /// Un serveur demarre avec `TUNE_PORT=9000` envoyait a Spotify une URI en
    /// 8888 : l'autorisation renvoie alors le navigateur vers un port ou rien
    /// n'ecoute. Le port n'est pas une constante, c'est une mesure.
    #[test]
    fn spotify_default_redirect_follows_the_port_actually_listened_on() {
        assert_eq!(
            resolve_redirect_uri(None, None, None, 9000),
            "http://127.0.0.1:9000/api/v1/streaming/spotify/callback"
        );
        assert_eq!(
            resolve_redirect_uri(None, None, None, 8085),
            "http://127.0.0.1:8085/api/v1/streaming/spotify/callback"
        );
        // L'hote reste un litteral de boucle locale a tout port : c'est la
        // seule forme que Spotify accepte encore en HTTP clair.
        for port in [80u16, 8085, 8888, 9000, 65535] {
            let uri = default_redirect_uri(port);
            assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
            assert!(!uri.contains("localhost"), "{uri}");
            assert!(
                uri.ends_with("/api/v1/streaming/spotify/callback"),
                "le chemin de callback ne doit jamais changer : {uri}"
            );
        }
    }

    /// Le port ne doit JAMAIS deplacer une URI que l'exploitant a ecrite.
    ///
    /// Une installation distante declare son URI HTTPS enregistree ; en
    /// deriver le port la casserait a chaque changement de `TUNE_PORT`.
    #[test]
    fn spotify_port_never_overrides_an_explicit_redirect() {
        assert_eq!(
            resolve_redirect_uri(Some("https://tune.example/cb"), None, None, 9000),
            "https://tune.example/cb"
        );
        assert_eq!(
            resolve_redirect_uri(None, Some("https://env.example/cb".into()), None, 9000),
            "https://env.example/cb"
        );
        assert_eq!(
            resolve_redirect_uri(None, None, Some("https://old.example/cb".into()), 9000),
            "https://old.example/cb"
        );
    }

    #[test]
    fn spotify_redirect_precedence_is_config_then_tune_env_then_legacy_env() {
        let tune_env = Some("https://env-tune.example/callback".into());
        let legacy_env = Some("https://env-legacy.example/callback".into());

        assert_eq!(
            resolve_redirect_uri(None, tune_env.clone(), legacy_env.clone(), DEFAULT_API_PORT),
            "https://env-tune.example/callback"
        );
        assert_eq!(
            resolve_redirect_uri(None, None, legacy_env.clone(), DEFAULT_API_PORT),
            "https://env-legacy.example/callback"
        );
        assert_eq!(
            resolve_redirect_uri(
                Some("https://config.example/callback"),
                tune_env,
                legacy_env,
                DEFAULT_API_PORT,
            ),
            "https://config.example/callback"
        );
    }

    /// #2680, fil 221 — « redirect_uri: Insecure ».
    ///
    /// Regle Spotify : HTTPS, sauf `http://127.0.0.1:<port>` ou
    /// `http://[::1]:<port>` ; `localhost` refuse. Le defaut n'est jamais
    /// refuse, a aucun port ; les deux URI conseillees le 19/05 sur le fil le
    /// sont toutes les deux.
    #[test]
    fn spotify_redirect_rule_accepts_only_https_or_explicit_loopback() {
        for port in [80u16, 8085, 8888, 9000, 65535] {
            let defaut = default_redirect_uri(port);
            assert_eq!(refus_redirection(&defaut), None, "{defaut}");
            assert_eq!(
                refus_redirection(&resolve_redirect_uri(None, None, None, port)),
                None
            );
        }
        for ok in [
            "http://127.0.0.1:8888/api/v1/streaming/spotify/callback",
            "http://[::1]:8888/api/v1/streaming/spotify/callback",
            "HTTP://127.0.0.1/api/v1/streaming/spotify/callback",
            "https://tune.example/api/v1/streaming/spotify/callback",
            "https://192.168.1.20:8443/api/v1/streaming/spotify/callback",
        ] {
            assert_eq!(refus_redirection(ok), None, "{ok}");
        }
        for (uri, refus) in [
            (
                "http://localhost:8888/api/v1/streaming/spotify/callback",
                RefusRedirection::Localhost,
            ),
            (
                "http://localhost:8085/api/v1/streaming/spotify/callback",
                RefusRedirection::Localhost,
            ),
            ("http://tune.localhost:8888/cb", RefusRedirection::Localhost),
            (
                "http://192.168.1.20:8888/callback/spotify",
                RefusRedirection::HttpHorsBouclage,
            ),
            (
                "http://10.0.0.5:8888/api/v1/streaming/spotify/callback",
                RefusRedirection::HttpHorsBouclage,
            ),
            (
                "http://nas.local:8888/api/v1/streaming/spotify/callback",
                RefusRedirection::HttpHorsBouclage,
            ),
            (
                "http://127.0.0.1.nip.io:8888/cb",
                RefusRedirection::HttpHorsBouclage,
            ),
        ] {
            assert_eq!(refus_redirection(uri), Some(refus), "{uri}");
        }
    }

    /// Une URI explicite refusee est CONSERVEE, pas reecrite : elle doit
    /// rester celle que l'exploitant a declaree.
    #[test]
    fn spotify_insecure_explicit_redirect_is_kept_as_is() {
        let lan = "http://192.168.1.20:8888/callback/spotify";
        assert_eq!(resolve_redirect_uri(Some(lan), None, None, 8888), lan);
        let svc = SpotifyService::with_config(Some("c"), Some(lan), 8888);
        assert_eq!(svc.redirect_uri, lan);
    }

    /// Depuis un autre poste, le rappel `127.0.0.1` ne s'ouvre pas, mais son
    /// adresse porte le `code` : Tune doit savoir l'en extraire.
    #[test]
    fn spotify_code_is_extracted_from_a_pasted_callback_url() {
        assert_eq!(
            code_depuis_rappel(
                "http://127.0.0.1:8888/api/v1/streaming/spotify/callback?code=AQB%2Dx_9&state=s"
            ),
            Ok("AQB-x_9".to_owned())
        );
        assert_eq!(
            code_depuis_rappel("  http://127.0.0.1:8888/cb?state=s&code=abc#frag "),
            Ok("abc".to_owned())
        );
        assert_eq!(code_depuis_rappel("AQBcodenu"), Ok("AQBcodenu".to_owned()));
        assert_eq!(
            code_depuis_rappel("http://127.0.0.1:8888/cb?error=access_denied"),
            Err("spotify: access_denied".to_owned())
        );
        assert!(code_depuis_rappel("http://127.0.0.1:8888/cb").is_err());
        assert!(code_depuis_rappel("").is_err());
    }

    /// Fil 221 (Yan Tasset) — le Client ID colle dans le champ de l'adresse
    /// de retour partait vers `api/token` comme un code : Spotify repondait
    /// `invalid_client`, et l'ecran accusait l'URI de redirection.
    #[test]
    fn fil_221_un_client_id_colle_n_est_pas_pris_pour_un_code() {
        let client_id = "0123456789abcdef0123456789ABCDEF";
        let err = code_depuis_rappel(client_id).expect_err("un Client ID n'est pas un code");
        assert!(err.contains("Client ID"), "{err}");
        assert!(err.contains("pas l'adresse de retour"), "{err}");
        // Entoure d'espaces, c'est toujours lui.
        assert!(code_depuis_rappel(&format!("  {client_id} ")).is_err());
        // Un vrai code (long, base64url) reste accepte tel quel.
        let code = "AQB".to_owned() + &"x_-9".repeat(60);
        assert_eq!(code_depuis_rappel(&code), Ok(code.clone()));
        // Le meme Client ID DANS une adresse de rappel n'est pas en cause.
        assert_eq!(
            code_depuis_rappel(&format!("http://127.0.0.1:8888/cb?code={client_id}")),
            Ok(client_id.to_owned())
        );
    }

    /// Fil 221 — sans Client ID, `authenticate` rendait une URL
    /// `client_id=placeholder` (« Ouvrir la page de connexion » menait a une
    /// erreur Spotify) et echangeait un code voue a `invalid_client`.
    #[tokio::test]
    async fn fil_221_sans_client_id_authenticate_refuse_en_le_disant() {
        for absent in ["placeholder", "", "   "] {
            let mut svc = SpotifyService::with_config(None, None, 8888);
            svc.set_client_id(absent);
            assert!(!svc.client_id_est_configure());
            let err = svc
                .authenticate(&json!({}))
                .await
                .expect_err("aucune URL d'autorisation sans Client ID");
            assert!(err.to_string().starts_with(CLIENT_ID_ABSENT), "{err}");
            assert!(err.to_string().contains("TUNE_SPOTIFY_CLIENT_ID"), "{err}");
            let err = svc
                .authenticate(&json!({"callback_url": "http://127.0.0.1:8888/cb?code=abc"}))
                .await
                .expect_err("aucun echange de code sans Client ID");
            assert!(err.to_string().starts_with(CLIENT_ID_ABSENT), "{err}");
        }
    }

    /// Fil 221 — le Client ID enregistre s'applique a chaud : l'URL
    /// d'autorisation suivante le porte, sans redemarrage.
    #[tokio::test]
    async fn fil_221_le_client_id_s_applique_a_chaud() {
        let mut svc = SpotifyService::with_config(None, None, 8888);
        svc.set_client_id("placeholder");
        svc.set_client_id(" 0123456789abcdef0123456789abcdef ");
        assert!(svc.client_id_est_configure());
        let st = svc
            .authenticate(&json!({}))
            .await
            .expect("URL d'autorisation");
        let url = st.verification_url.expect("une URL");
        assert!(
            url.contains("client_id=0123456789abcdef0123456789abcdef"),
            "{url}"
        );
        assert_eq!(svc.tentatives.len(), 1);
        // Changer d'identifiant oublie l'echange PKCE entame avec l'ancien.
        svc.set_client_id("fedcba9876543210fedcba9876543210");
        assert!(svc.tentatives.is_empty());
    }

    /// Fil 221 — l'ordre : la variable d'environnement, puis le reglage en
    /// base, puis `tune.toml`. Vide et `placeholder` comptent comme absents.
    #[test]
    fn fil_221_ordre_de_resolution_du_client_id() {
        assert_eq!(
            resolve_client_id(Some("env"), Some("base"), Some("toml")).as_deref(),
            Some("env")
        );
        assert_eq!(
            resolve_client_id(None, Some("base"), Some("toml")).as_deref(),
            Some("base")
        );
        assert_eq!(
            resolve_client_id(Some(""), Some(" base "), Some("toml")).as_deref(),
            Some("base")
        );
        assert_eq!(
            resolve_client_id(None, Some("placeholder"), Some("toml")).as_deref(),
            Some("toml")
        );
        assert_eq!(resolve_client_id(None, None, Some("placeholder")), None);
        assert_eq!(resolve_client_id(None, None, None), None);
    }

    /// Le chemin `callback_url` passe bien par l'echange de code : sans
    /// verificateur PKCE (aucune autorisation lancee), il echoue sur CE motif,
    /// et non comme un simple sondage qui rendrait une nouvelle URL.
    #[tokio::test]
    async fn spotify_callback_url_credential_reaches_the_code_exchange() {
        let mut svc = SpotifyService::with_config(Some("c"), None, 8888);
        let err = svc
            .authenticate(&json!({"callback_url": "http://127.0.0.1:8888/cb?code=abc"}))
            .await
            .expect_err("sans verificateur, l'echange doit echouer");
        assert!(err.to_string().contains("no code verifier"), "{err}");
        let err = svc
            .authenticate(&json!({"callback_url": "http://127.0.0.1:8888/cb?error=access_denied"}))
            .await
            .expect_err("un refus colle doit etre rendu");
        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    #[test]
    fn spotify_explicit_redirect_is_used_in_the_authorization_url() {
        let svc = SpotifyService::with_config(
            Some("client-test"),
            Some("https://tune.example/api/v1/streaming/spotify/callback"),
            DEFAULT_API_PORT,
        );
        let url = svc.auth_url("challenge-test", "etat-test", &svc.redirect_uri);

        assert!(url.contains("client_id=client-test"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Ftune.example%2Fapi%2Fv1%2Fstreaming%2Fspotify%2Fcallback"
        ));
    }

    #[test]
    fn map_track_basic() {
        let json = json!({
            "id": "track123",
            "name": "Bohemian Rhapsody",
            "artists": [{"name": "Queen", "id": "artist1"}],
            "album": {
                "name": "A Night at the Opera",
                "id": "album1",
                "images": [{"url": "http://img.spotify.com/cover.jpg"}],
            },
            "duration_ms": 354000,
            "track_number": 11,
            "disc_number": 1,
            "explicit": false,
        });
        let track = SpotifyService::map_track(&json);
        assert_eq!(track.id, "track123");
        assert_eq!(track.title, "Bohemian Rhapsody");
        assert_eq!(track.artist, "Queen");
        assert_eq!(track.album.as_deref(), Some("A Night at the Opera"));
        assert_eq!(track.album_id.as_deref(), Some("album1"));
        assert_eq!(track.duration_ms, 354000);
        assert_eq!(track.track_number, Some(11));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert_eq!(
            track.cover_path.as_deref(),
            Some("http://img.spotify.com/cover.jpg")
        );
        let q = track.quality.unwrap();
        assert_eq!(q.codec, "OGG");
        assert_eq!(q.bitrate, Some(320));
    }

    #[test]
    fn map_track_explicit() {
        let json = json!({
            "id": "x",
            "name": "Explicit",
            "artists": [{"name": "Test"}],
            "album": {},
            "duration_ms": 100000,
            "explicit": true,
        });
        let track = SpotifyService::map_track(&json);
        assert!(track.explicit);
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": null,
            "name": null,
            "album": {},
        });
        let track = SpotifyService::map_track(&json);
        assert_eq!(track.id, "");
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": "album123",
            "name": "A Night at the Opera",
            "artists": [{"name": "Queen", "id": "artist1"}],
            "images": [{"url": "http://img.spotify.com/album.jpg"}],
            "release_date": "1975-11-21",
            "total_tracks": 12,
        });
        let album = SpotifyService::map_album(&json);
        assert_eq!(album.id, "album123");
        assert_eq!(album.title, "A Night at the Opera");
        assert_eq!(album.artist, "Queen");
        assert_eq!(album.artist_id.as_deref(), Some("artist1"));
        assert_eq!(album.year, Some(1975));
        assert_eq!(album.track_count, 12);
        assert_eq!(
            album.cover_path.as_deref(),
            Some("http://img.spotify.com/album.jpg")
        );
    }

    #[test]
    fn map_album_year_only() {
        let json = json!({
            "id": "a",
            "name": "Test",
            "artists": [],
            "release_date": "2023",
            "total_tracks": 1,
        });
        let album = SpotifyService::map_album(&json);
        assert_eq!(album.year, Some(2023));
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": "artist123",
            "name": "Queen",
            "images": [{"url": "http://img.spotify.com/artist.jpg"}],
        });
        let artist = SpotifyService::map_artist(&json);
        assert_eq!(artist.id, "artist123");
        assert_eq!(artist.name, "Queen");
        assert_eq!(
            artist.image_path.as_deref(),
            Some("http://img.spotify.com/artist.jpg")
        );
    }

    #[test]
    fn map_artist_no_images() {
        let json = json!({
            "id": "a",
            "name": "Unknown",
            "images": [],
        });
        let artist = SpotifyService::map_artist(&json);
        assert!(artist.image_path.is_none());
    }

    #[test]
    fn spotify_service_name() {
        let svc = SpotifyService::new();
        assert_eq!(svc.name(), "spotify");
    }

    #[test]
    fn spotify_save_tokens_no_auth() {
        let svc = SpotifyService::new();
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn spotify_restore_tokens() {
        let mut svc = SpotifyService::new();
        let tokens = json!({
            "access_token": "spotify-token",
            "refresh_token": "refresh-token",
            "username": "testuser",
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.access_token.as_deref(), Some("spotify-token"));
        assert_eq!(svc.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
    }

    #[test]
    fn spotify_restore_tokens_invalid() {
        let mut svc = SpotifyService::new();
        let tokens = json!({"nothing": "here"});
        assert!(!svc.restore_tokens(&tokens));
    }

    // ------------------------------------------------------------------
    // #2851 — le jeton restauré ne doit plus être cru valide une heure
    // ------------------------------------------------------------------

    /// Le défaut du ticket : `restore_tokens` fabriquait `now + 3600` pour un
    /// jeton dont il ignore l'âge. La ligne persistée ne porte aucune date —
    /// l'échéance doit rester inconnue.
    #[test]
    fn spotify_restauration_laisse_l_echeance_inconnue() {
        let mut svc = SpotifyService::new();
        assert!(svc.restore_tokens(&json!({
            "access_token": "at",
            "refresh_token": "rt",
        })));
        assert!(
            svc.token_expires.is_none(),
            "#2851 : la restauration ne doit inventer aucune échéance — \
             rien dans la ligne persistée ne dit quand le jeton a été émis"
        );
    }

    /// Conséquence directe : au premier tick du rafraîchisseur (300 s), le
    /// jeton restauré est renouvelé au lieu d'être cru bon 55 minutes.
    #[test]
    fn spotify_echeance_inconnue_declenche_le_rafraichissement() {
        let mut svc = SpotifyService::new();
        assert!(svc.restore_tokens(&json!({
            "access_token": "at",
            "refresh_token": "rt",
        })));
        assert!(
            spotify_needs_refresh(
                svc.token_expires,
                svc.access_token.is_some(),
                svc.refresh_token.is_some(),
                svc.token_rejected.load(Ordering::Relaxed),
            ),
            "#2851 : échéance inconnue ⇒ rafraîchir, comme Tidal et YouTube — \
             la convention inverse neutralisait le rafraîchisseur périodique"
        );
    }

    /// TÉMOIN anti-régression : un jeton réellement valide n'est PAS
    /// rafraîchi inutilement. Vert avec comme sans le correctif.
    #[test]
    fn spotify_jeton_valide_n_est_pas_rafraichi() {
        let dans_une_heure = Some(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        assert!(
            !spotify_needs_refresh(dans_une_heure, true, true, false),
            "un jeton valide une heure encore ne doit pas être renouvelé"
        );
    }

    /// TÉMOIN : la marge de 300 s existante est préservée.
    #[test]
    fn spotify_marge_de_cinq_minutes_preservee() {
        let bientot = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        assert!(spotify_needs_refresh(bientot, true, true, false));
        let deja_passe = Some(std::time::Instant::now());
        assert!(spotify_needs_refresh(deja_passe, true, true, false));
    }

    /// TÉMOIN : sans jeton de rafraîchissement, on ne boucle pas sur un
    /// renouvellement impossible à chaque tick.
    #[test]
    fn spotify_sans_refresh_token_aucun_rafraichissement() {
        assert!(!spotify_needs_refresh(None, true, false, false));
        assert!(!spotify_needs_refresh(None, false, true, false));
    }

    /// Second volet du ticket : les quatre méthodes HTTP retombaient en
    /// `Err("token expired")` sur un 401 sans que rien ne tente un
    /// renouvellement. Le refus est désormais relayé jusqu'au tick suivant.
    #[test]
    fn spotify_401_programme_un_rafraichissement() {
        let svc = SpotifyService::new();
        assert!(
            !svc.token_rejected.load(Ordering::Relaxed),
            "aucun refus au départ"
        );

        // Ce que fait chacune des quatre méthodes HTTP en voyant un 401.
        svc.note_token_rejected();

        assert!(svc.token_rejected.load(Ordering::Relaxed));
        let dans_une_heure = Some(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        assert!(
            spotify_needs_refresh(dans_une_heure, true, true, true),
            "#2851 : un 401 doit déclencher un renouvellement même si \
             l'échéance en mémoire prétend que le jeton est encore bon"
        );
    }

    /// Le refus relayé ne peut pas provoquer un renouvellement impossible.
    #[test]
    fn spotify_401_sans_refresh_token_ne_boucle_pas() {
        assert!(!spotify_needs_refresh(None, true, false, true));
    }

    /// Se déconnecter efface le refus et l'échéance : la session suivante
    /// repart d'un état neuf.
    #[tokio::test]
    async fn spotify_logout_efface_le_refus_et_l_echeance() {
        let mut svc = SpotifyService::new();
        svc.access_token = Some("at".into());
        svc.refresh_token = Some("rt".into());
        svc.token_expires = Some(std::time::Instant::now());
        svc.note_token_rejected();

        svc.logout().await.unwrap();

        assert!(svc.token_expires.is_none());
        assert!(!svc.token_rejected.load(Ordering::Relaxed));
        assert!(!spotify_needs_refresh(
            svc.token_expires,
            svc.access_token.is_some(),
            svc.refresh_token.is_some(),
            svc.token_rejected.load(Ordering::Relaxed),
        ));
    }

    #[test]
    fn spotify_set_enabled() {
        let mut svc = SpotifyService::new();
        svc.set_enabled(true);
        assert!(svc.enabled());
        svc.set_enabled(false);
        assert!(!svc.enabled());
    }

    #[test]
    fn base64url_encode_known() {
        let data = b"Hello";
        let result = base64url_encode(data);
        assert_eq!(result, "SGVsbG8");
    }

    #[test]
    fn spotify_save_restore_user_id() {
        let mut svc = SpotifyService::new();
        svc.access_token = Some("token".into());
        svc.user_id = Some("user123".into());
        svc.username = Some("Display Name".into());

        let tokens = svc.save_tokens().unwrap();
        assert_eq!(tokens["user_id"], "user123");

        let mut svc2 = SpotifyService::new();
        assert!(svc2.restore_tokens(&tokens));
        assert_eq!(svc2.user_id.as_deref(), Some("user123"));
        assert!(svc2.supports_write());
    }

    #[test]
    fn spotify_supports_write_requires_both() {
        let mut svc = SpotifyService::new();
        assert!(!svc.supports_write());

        svc.access_token = Some("token".into());
        assert!(!svc.supports_write());

        svc.user_id = Some("user123".into());
        assert!(svc.supports_write());
    }

    // ── Fil 221 : le verificateur PKCE survit a l'attente ─────────────────
    //
    // Doublure locale de `accounts.spotify.com/api/token` et de `/v1/me` :
    // aucune requete ne part vers Spotify. Comme Spotify, elle refuse un
    // `code_verifier` dont le SHA-256 n'est pas le `code_challenge` du lien
    // d'autorisation, et un code deja servi.

    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Doublure {
        /// `code` -> `code_challenge` du lien qui l'a produit.
        codes: Arc<Mutex<Vec<(String, String)>>>,
        servis: Arc<Mutex<Vec<String>>>,
    }

    fn parametre(url: &str, cle: &str) -> Option<String> {
        let requete = url.split_once('?')?.1;
        requete.split('&').find_map(|p| {
            let (k, v) = p.split_once('=')?;
            (k == cle).then(|| urlencoding::decode(v).unwrap().into_owned())
        })
    }

    async fn jeton_double(
        axum::extract::State(d): axum::extract::State<Doublure>,
        corps: String,
    ) -> axum::Json<serde_json::Value> {
        let champ = |cle: &str| parametre(&format!("?{corps}"), cle).unwrap_or_default();
        let (code, verifier) = (champ("code"), champ("code_verifier"));
        let refus = |d: &str| json!({"error": "invalid_grant", "error_description": d});
        if d.servis.lock().unwrap().contains(&code) {
            return axum::Json(refus("Invalid authorization code"));
        }
        let attendu = d
            .codes
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, ch)| ch.clone());
        let Some(attendu) = attendu else {
            return axum::Json(refus("Invalid authorization code"));
        };
        d.servis.lock().unwrap().push(code);
        if base64url_encode(&Sha256::digest(verifier.as_bytes())) != attendu {
            return axum::Json(refus("code_verifier was incorrect"));
        }
        axum::Json(json!({"access_token": "a", "refresh_token": "r", "expires_in": 3600}))
    }

    async fn doublure() -> (String, Doublure) {
        let d = Doublure::default();
        let app = axum::Router::new()
            .route("/api/token", axum::routing::post(jeton_double))
            .route(
                "/v1/me",
                axum::routing::get(|| async {
                    axum::Json(json!({"display_name": "Essai", "id": "u1"}))
                }),
            )
            .with_state(d.clone());
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        (base, d)
    }

    /// Ouvre une autorisation ; rend le `state` du lien. Spotify, apres
    /// consentement, emettra `code` pour CE lien (donc pour son challenge).
    async fn ouvrir(svc: &mut SpotifyService, d: &Doublure, code: &str) -> String {
        let url = svc
            .authenticate(&json!({"device_flow": true}))
            .await
            .unwrap()
            .verification_url
            .expect("lien d'autorisation");
        let challenge = parametre(&url, "code_challenge").expect("code_challenge");
        d.codes.lock().unwrap().push((code.into(), challenge));
        parametre(&url, "state").expect("le lien porte un state")
    }

    fn retour(code: &str, state: Option<&str>) -> serde_json::Value {
        let mut url = format!("http://127.0.0.1:8888{REDIRECT_PATH}?code={code}");
        if let Some(state) = state {
            url.push_str(&format!("&state={state}"));
        }
        json!({"callback_url": url})
    }

    async fn service_double() -> (SpotifyService, Doublure) {
        let (base, d) = doublure().await;
        let svc =
            SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
        (svc, d)
    }

    /// Le cas du fil : l'ecran sonde `GET /status` toutes les 3 s pendant
    /// qu'on consent sur Spotify et qu'on recopie l'adresse 127.0.0.1. Ces
    /// sondages ne doivent pas changer le verificateur.
    #[tokio::test]
    async fn fil_221_les_sondages_n_ecrasent_pas_le_verificateur() {
        for avec_state in [true, false] {
            let (mut svc, d) = service_double().await;
            let state = ouvrir(&mut svc, &d, "code-1").await;
            for _ in 0..10 {
                let st = svc.authenticate(&json!({"poll": true})).await.unwrap();
                assert!(!st.authenticated);
                assert!(
                    st.verification_url.is_none(),
                    "un sondage ne relance pas l'autorisation"
                );
            }
            let st = svc
                .authenticate(&retour("code-1", avec_state.then_some(state.as_str())))
                .await
                .unwrap_or_else(|e| {
                    panic!("echange apres 30 s d'attente (state: {avec_state}) : {e}")
                });
            assert!(st.authenticated);
            assert!(
                svc.tentatives.is_empty(),
                "un echange reussi solde les tentatives"
            );
        }
    }

    /// Deux clics sur « Se connecter » (ou deux onglets) : le retour du
    /// PREMIER lien s'echange encore avec son propre verificateur.
    #[tokio::test]
    async fn fil_221_deux_demarrages_ne_s_ecrasent_pas() {
        let (mut svc, d) = service_double().await;
        let premier = ouvrir(&mut svc, &d, "code-a").await;
        let _second = ouvrir(&mut svc, &d, "code-b").await;
        let st = svc
            .authenticate(&retour("code-a", Some(&premier)))
            .await
            .unwrap_or_else(|e| panic!("le premier lien doit aboutir : {e}"));
        assert!(st.authenticated);
    }

    /// Le second clic sur « Valider » : le premier essai ne doit pas avoir
    /// consomme la tentative (`no code verifier`). Le code, lui, est brule :
    /// on le dit, et un nouveau code du meme lien aboutit.
    #[tokio::test]
    async fn fil_221_un_echec_ne_consomme_pas_la_tentative() {
        let (mut svc, d) = service_double().await;
        let state = ouvrir(&mut svc, &d, "code-1").await;
        // Le code est servi une premiere fois ailleurs (rappel automatique,
        // premier « Valider ») : Spotify le refuse ensuite.
        d.servis.lock().unwrap().push("code-1".into());
        let err = svc
            .authenticate(&retour("code-1", Some(&state)))
            .await
            .expect_err("un code deja servi est refuse")
            .to_string();
        assert!(err.contains("Invalid authorization code"), "{err}");
        assert!(err.contains("un code ne sert qu'une fois"), "{err}");
        let err = svc
            .authenticate(&retour("code-1", Some(&state)))
            .await
            .expect_err("toujours brule")
            .to_string();
        assert!(
            !err.contains("no code verifier"),
            "le second clic ne doit pas perdre la tentative : {err}"
        );
        // Rouvrir le MEME lien rend un nouveau code, pour le meme challenge.
        let challenge = d.codes.lock().unwrap()[0].1.clone();
        d.codes.lock().unwrap().push(("code-2".into(), challenge));
        let st = svc
            .authenticate(&retour("code-2", Some(&state)))
            .await
            .unwrap();
        assert!(st.authenticated);
    }

    /// Une adresse d'une autre tentative (ou d'avant un redemarrage) est
    /// reconnue et nommee, au lieu d'un `invalid_grant` opaque.
    #[tokio::test]
    async fn fil_221_adresse_d_une_autre_tentative() {
        let (mut svc, d) = service_double().await;
        let err = svc
            .authenticate(&retour("code-x", Some("etat-inconnu")))
            .await
            .expect_err("aucune tentative en cours")
            .to_string();
        assert!(err.contains("aucune connexion en cours"), "{err}");
        ouvrir(&mut svc, &d, "code-1").await;
        let err = svc
            .authenticate(&retour("code-1", Some("etat-inconnu")))
            .await
            .expect_err("state etranger")
            .to_string();
        assert!(err.contains("autre tentative"), "{err}");
    }

    #[test]
    fn fil_221_rappel_colle_garde_le_state() {
        assert_eq!(
            rappel_depuis_colle("http://127.0.0.1:8888/cb?code=abc&state=xyz").unwrap(),
            RappelSpotify {
                code: "abc".into(),
                state: Some("xyz".into())
            }
        );
        assert_eq!(
            rappel_depuis_colle("http://127.0.0.1:8888/cb?state=xyz&code=abc#")
                .unwrap()
                .code,
            "abc"
        );
        assert_eq!(rappel_depuis_colle("abc").unwrap().state, None);
    }

    #[test]
    fn fil_221_verificateur_pkce_aleatoire_et_conforme() {
        let (v1, c1) = SpotifyService::generate_pkce();
        let (v2, _) = SpotifyService::generate_pkce();
        assert_ne!(v1, v2);
        assert_eq!(v1.len(), 128);
        assert!(
            v1.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(c1, base64url_encode(&Sha256::digest(v1.as_bytes())));
    }

    /// #6018 — l'appareil librespot est désigné par son NOM, et l'API Web lui
    /// demande de jouer le titre à la position voulue.
    #[tokio::test]
    async fn spotify_6018_lancer_sur_l_appareil_designe_librespot() {
        #[derive(Clone, Default)]
        struct Vu(Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>);
        let vu = Vu::default();
        let app = axum::Router::new()
            .route(
                "/v1/me/player/devices",
                axum::routing::get(|| async {
                    axum::Json(json!({"devices": [
                        {"id": "dev-salon", "name": "Salon", "is_active": true},
                        {"id": "dev-tune", "name": "Tune", "is_active": false}
                    ]}))
                }),
            )
            .route(
                "/v1/me/player/play",
                axum::routing::put(
                    |axum::extract::State(v): axum::extract::State<Vu>,
                     axum::extract::RawQuery(q): axum::extract::RawQuery,
                     corps: axum::Json<serde_json::Value>| async move {
                        v.0.lock().unwrap().push((q.unwrap_or_default(), corps.0));
                        axum::http::StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(vu.clone());
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let mut svc =
            SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
        svc.access_token = Some("jeton".into());

        assert_eq!(
            svc.appareil_par_nom("Tune").await.unwrap().as_deref(),
            Some("dev-tune")
        );
        svc.lancer_sur_l_appareil("Tune", "4uLU6hMCjMI75M1A2tKUQC", 5000)
            .await
            .expect("lecture lancée");
        let vu = vu.0.lock().unwrap().clone();
        assert_eq!(vu.len(), 1);
        assert_eq!(vu[0].0, "device_id=dev-tune");
        assert_eq!(
            vu[0].1,
            json!({"uris": ["spotify:track:4uLU6hMCjMI75M1A2tKUQC"], "position_ms": 5000})
        );
    }

    /// #6825 — un refus de l'API Web sur une rubrique éditoriale est une
    /// erreur, avec son statut et le motif de Spotify, et non une liste vide.
    /// Une réponse 200 garde son contenu.
    #[tokio::test]
    async fn spotify_6825_un_refus_editorial_est_une_erreur_et_non_une_liste_vide() {
        let app = axum::Router::new()
            .route(
                "/v1/browse/featured-playlists",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(json!({"error": {"status": 404, "message": "Not Found"}})),
                    )
                }),
            )
            .route(
                "/v1/browse/new-releases",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::FORBIDDEN,
                        axum::Json(json!({"error": {"status": 403, "message": "Forbidden"}})),
                    )
                }),
            )
            .route(
                "/v1/browse/categories",
                axum::routing::get(|| async {
                    axum::Json(json!({"categories": {"items": [
                        {"id": "jazz", "name": "Jazz", "icons": []}
                    ]}}))
                }),
            );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let mut svc =
            SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
        svc.access_token = Some("jeton".into());

        let err = svc
            .get_new_releases()
            .await
            .expect_err("#6825 : un 403 de /browse/new-releases rendait une liste vide")
            .to_string();
        assert!(err.contains("403") && err.contains("Forbidden"), "{err}");
        assert!(!err.contains("jeton"), "le jeton ne sort jamais : {err}");
        let err = svc
            .get_featured()
            .await
            .expect_err("#6825 : un 404 de /browse/featured-playlists rendait une liste vide")
            .to_string();
        assert!(err.contains("404"), "{err}");
        let genres = svc.get_genres(None).await.expect("200");
        assert_eq!(genres.len(), 1);
    }

    /// #8399 — fil 2750 : Spotify ferme `/browse/featured-playlists` aux
    /// applications récentes (403). La rubrique est vide, sans erreur (le web
    /// la masque), et Spotify n'est plus interrogé à chaque ouverture.
    #[tokio::test]
    async fn spotify_8399_la_selection_refusee_en_403_est_vide_sans_erreur_ni_rappel() {
        use std::sync::atomic::AtomicUsize;
        static APPELS: AtomicUsize = AtomicUsize::new(0);
        let app = axum::Router::new().route(
            "/v1/browse/featured-playlists",
            axum::routing::get(|| async {
                APPELS.fetch_add(1, Ordering::SeqCst);
                (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::Json(json!({"error": {"status": 403, "message": "Forbidden"}})),
                )
            }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let mut svc =
            SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
        svc.access_token = Some("jeton".into());

        for _ in 0..3 {
            let liste = svc
                .get_featured()
                .await
                .expect("#8399 : un 403 ne remonte plus en erreur (502 à l'écran)");
            assert!(liste.is_empty());
        }
        assert_eq!(
            APPELS.load(Ordering::SeqCst),
            1,
            "un seul appel, puis mémorisé"
        );

        // Une autre application (Client ID) peut y avoir droit : Spotify est
        // de nouveau interrogé.
        svc.set_client_id("client-ancien");
        svc.access_token = Some("jeton".into());
        assert!(svc.get_featured().await.expect("403 → vide").is_empty());
        assert_eq!(
            APPELS.load(Ordering::SeqCst),
            2,
            "redemandé après changement d'application"
        );
    }

    #[test]
    fn spotify_8399_statut_du_refus() {
        assert_eq!(
            statut_du_refus("spotify GET /browse/featured-playlists: HTTP 403 — Forbidden"),
            Some(403)
        );
        assert_eq!(
            statut_du_refus("spotify GET /x: HTTP 404 — Not Found"),
            Some(404)
        );
        assert_eq!(statut_du_refus("token expired"), None);
    }

    #[test]
    fn spotify_6825_motif_du_refus_borne() {
        assert_eq!(
            motif_du_refus(r#"{"error": {"status": 403, "message": "Forbidden"}}"#),
            "Forbidden"
        );
        assert_eq!(
            motif_du_refus("  <html>bad gateway</html> "),
            "<html>bad gateway</html>"
        );
        assert_eq!(motif_du_refus(&"x".repeat(500)).chars().count(), 200);
        assert_eq!(motif_du_refus(""), "");
    }

    /// Sans Premium, Spotify répond 403 : Tune le dit en clair.
    #[tokio::test]
    async fn spotify_6018_un_refus_403_parle_de_premium() {
        let app = axum::Router::new()
            .route(
                "/v1/me/player/devices",
                axum::routing::get(|| async {
                    axum::Json(json!({"devices": [{"id": "dev-tune", "name": "Tune"}]}))
                }),
            )
            .route(
                "/v1/me/player/play",
                axum::routing::put(|| async {
                    (
                        axum::http::StatusCode::FORBIDDEN,
                        axum::Json(json!({"error": {"status": 403, "message": "Player command failed: Premium required", "reason": "PREMIUM_REQUIRED"}})),
                    )
                }),
            );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let mut svc =
            SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
        svc.access_token = Some("jeton".into());
        let err = svc
            .lancer_sur_l_appareil("Tune", "x", 0)
            .await
            .expect_err("403");
        assert!(err.contains("Premium"), "{err}");
    }

    /// Piloter l'appareil et connecter librespot exigent ces portées.
    #[test]
    fn spotify_6018_les_portees_permettent_la_lecture() {
        for portee in [
            "streaming",
            "user-read-playback-state",
            "user-modify-playback-state",
        ] {
            assert!(
                SCOPES.split(' ').any(|p| p == portee),
                "portée OAuth manquante : {portee}"
            );
        }
    }

    /// #6933 — le rafraîchissement garde les portées ACCORDÉES par Spotify
    /// (champ `scope`), et la sauvegarde les transmet : c'est ce qui permet
    /// de dire si « reconnecter » réglera un refus de librespot.
    #[tokio::test]
    async fn spotify_6933_les_portees_accordees_sont_gardees() {
        let app = axum::Router::new().route(
            "/api/token",
            axum::routing::post(|| async {
                axum::Json(json!({
                    "access_token": "neuf",
                    "expires_in": 3600,
                    "scope": "user-read-private streaming user-modify-playback-state"
                }))
            }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let mut svc =
            SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
        assert!(svc.restore_tokens(&json!({"access_token": "vieux", "refresh_token": "r"})));
        assert_eq!(
            svc.porte_la_lecture(),
            None,
            "portées inconnues avant rafraîchissement"
        );
        assert!(svc.refresh_if_needed().await.unwrap());
        assert_eq!(
            svc.porte_la_lecture(),
            Some(true),
            "portées accordées au rafraîchissement non gardées"
        );

        let sauvegarde = svc.save_tokens().unwrap();
        let mut relu = SpotifyService::new();
        assert!(relu.restore_tokens(&sauvegarde));
        assert_eq!(
            relu.porte_la_lecture(),
            Some(true),
            "portées perdues à la sauvegarde"
        );

        let mut ancien = SpotifyService::new();
        assert!(ancien.restore_tokens(&json!({
            "access_token": "a", "refresh_token": "r", "scope": "user-read-private user-library-read"
        })));
        assert_eq!(ancien.porte_la_lecture(), Some(false));
    }

    /// #6933 — un 403 de région ou de contenu n'invite pas à se reconnecter ;
    /// seule une portée manquante le fait.
    #[test]
    fn spotify_6933_un_403_ne_demande_de_se_reconnecter_que_pour_une_portee() {
        let region = motif_refus_403(
            &json!({"error": {"status": 403, "message": "Player command failed: Restriction violated", "reason": "UNKNOWN"}}).to_string(),
        );
        assert!(!region.to_lowercase().contains("reconnect"), "{region}");
        assert!(region.contains("Restriction violated"), "{region}");
        let premium = motif_refus_403(
            &json!({"error": {"status": 403, "message": "Player command failed: Premium required", "reason": "PREMIUM_REQUIRED"}}).to_string(),
        );
        assert!(
            premium.contains("Premium") && !premium.to_lowercase().contains("reconnect"),
            "{premium}"
        );
        let portee = motif_refus_403(
            &json!({"error": {"status": 403, "message": "Insufficient client scope"}}).to_string(),
        );
        assert!(portee.contains("reconnecter"), "{portee}");
        assert!(
            !motif_appareil_absent("Tune")
                .to_lowercase()
                .contains("reconnect")
        );
    }
}

#[cfg(test)]
#[path = "spotify_recherche_7659_tests.rs"]
mod recherche_7659_tests;

#[cfg(test)]
#[path = "spotify_relais_8198_tests.rs"]
mod relais_8198_tests;
#[cfg(test)]
#[path = "spotify_session_refusee_8051_tests.rs"]
mod session_refusee_8051_tests;

#[cfg(test)]
#[path = "spotify_favoris_pagination_8050_tests.rs"]
mod favoris_pagination_8050_tests;

#[cfg(test)]
#[path = "spotify_playlists_items_8193_tests.rs"]
mod playlists_items_8193_tests;

#[cfg(test)]
#[path = "spotify_rubriques_retirees_2026_tests.rs"]
mod spotify_rubriques_retirees_2026_tests;

#[cfg(test)]
#[path = "spotify_titres_phares_8485_tests.rs"]
mod titres_phares_8485_tests;

#[cfg(test)]
#[path = "spotify_albums_artiste_8479_tests.rs"]
mod albums_artiste_8479_tests;
