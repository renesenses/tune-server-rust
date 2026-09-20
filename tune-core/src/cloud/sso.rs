use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::cloud::rate_limit::{self, AppelCloud, CloudScope};
use crate::db::settings_repo::SettingsRepo;

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// Fenêtre de throttle retenue quand la réponse 429 ne porte ni `Retry-After`
/// ni `X-RateLimit-Reset`.
///
/// Mesuré sur `https://mozaiklabs.fr/api/v1/user` : `x-ratelimit-limit: 30`, un
/// jeton consommé par requête et par client, remis à zéro en une minute — le
/// `throttle` Laravel par défaut. Le délai n'est PAS inventé côté persistance
/// (`defer_from_headers` n'écrit rien sans en-tête, et cela ne change pas) : ce
/// repli ne sert qu'à DIRE à l'utilisateur dans combien de temps réessayer,
/// plutôt que de lui rendre un « 0 » ou un silence.
pub const FENETRE_THROTTLE_DEFAUT_S: u64 = 60;

/// Baked-in OAuth client id for the public **PKCE** "Tune" client on
/// mozaiklabs.fr.
///
/// This is the public `tune-server` client (RFC 7636 PKCE, no secret): safe to
/// distribute in every binary. Tune must still keep working 100 % without
/// mozaiklabs.fr — the SSO login remains opt-in for the user, never blocking.
///
/// Runtime overrides (see `tune-server` route resolution): the `mozaik_client_id`
/// setting or the `TUNE_MOZAIK_CLIENT_ID` env var take precedence over this const.
pub const DEFAULT_CLIENT_ID: &str = "tune-server";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudUser {
    pub id: i64,
    pub email: String,
    pub display_name: String,
    pub is_admin: bool,
    pub avatar_url: Option<String>,
    /// Premium granted by this mozaiklabs.fr account (derived server-side from the
    /// email-linked License). Absent on older servers → defaults to false.
    #[serde(default)]
    pub premium: bool,
    /// Subscription end for the account premium (ISO-8601), when known.
    #[serde(default)]
    pub license_expires_at: Option<String>,
    /// Qobuz endpoint order for this account: `true` = route Qobuz through the
    /// mozaiklabs proxy first (founder account). Absent on older servers →
    /// defaults to false (direct-first for every user).
    #[serde(default)]
    pub qobuz_proxy_first: bool,
    /// Paid MODULE entitlements owned by this account, as stable module ids
    /// (e.g. "diretta"). Separate SKUs, NOT implied by `premium`: a module is
    /// sold on its own and a premium account owns none by default. Absent on
    /// older servers → empty (no modules).
    #[serde(default)]
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

/// PKCE (RFC 7636) parameters for a single authorization flow.
///
/// Created at `/sso/authorize`, persisted for the duration of the browser
/// round-trip (keyed by `state`), and consumed at `/sso/callback`. The public
/// client sends no secret: the `verifier` proves it initiated the flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkceSession {
    /// Opaque CSRF token echoed back by the authorization server.
    pub state: String,
    /// High-entropy secret kept locally, replayed at token exchange.
    pub verifier: String,
    /// `base64url(sha256(verifier))`, sent in the authorize request (S256).
    pub challenge: String,
}

impl PkceSession {
    /// Generate a fresh PKCE session: random verifier, S256 challenge, CSRF state.
    pub fn generate() -> Self {
        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);
        let state = generate_state();
        Self {
            state,
            verifier,
            challenge,
        }
    }
}

/// Verdict de la relecture du profil du compte lié (`GET /api/v1/user`).
///
/// 🔴 Pourquoi un verdict à trois branches et non un `Result` : le profil
/// porte `modules`, les droits de MODULE payants (la sortie Diretta, par
/// exemple), et `tune-server/src/discovery_setup.rs` garde la découverte des
/// sorties derrière eux. Confondre « le cloud m'a dit non » avec « le cloud
/// m'a dit *pas maintenant* » revient à faire disparaître un appareil payé
/// parce que trente requêtes sont passées dans la minute. L'appelant doit
/// pouvoir distinguer les deux — c'est la raison d'être de [`Self::Differe`].
#[derive(Debug)]
pub enum ProfilCloud {
    /// Le profil a été relu : il fait foi.
    Profil(Box<CloudUser>),
    /// Throttle : l'appel n'est pas parti, ou il a été refusé par un 429.
    /// ⛔ Ce n'est PAS un échec du compte : l'appelant ne doit RIEN réécrire
    /// (ni `premium`, ni `modules`) et doit réessayer plus tard.
    Differe {
        /// Délai avant de réessayer, d'après `Retry-After` /
        /// `X-RateLimit-Reset`, à défaut [`FENETRE_THROTTLE_DEFAUT_S`].
        retry_after_seconds: u64,
    },
    /// Vrai échec : 401, 500, corps illisible, hôte injoignable. Réessayer
    /// tout de suite n'y changerait rien.
    Echec(String),
}

impl ProfilCloud {
    /// Le profil quand il a pu être relu. `None` couvre les deux non-réponses :
    /// seul le porteur du verdict peut les distinguer.
    pub fn profil(self) -> Option<CloudUser> {
        match self {
            Self::Profil(u) => Some(*u),
            _ => None,
        }
    }
}

pub struct MozaikAuth {
    pub client_id: String,
    base_url: String,
}

impl MozaikAuth {
    pub fn new(client_id: String, base_url: Option<&str>) -> Self {
        Self {
            client_id,
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// Build the OAuth2 **PKCE** authorize URL that the browser is redirected to.
    ///
    /// Public client: no secret is transmitted, only the S256 `code_challenge`
    /// and a CSRF `state`.
    pub fn authorize_url(&self, redirect_uri: &str, challenge: &str, state: &str) -> String {
        format!(
            "{}/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&code_challenge={}&code_challenge_method=S256&state={}",
            self.base_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(challenge),
            urlencoding::encode(state),
        )
    }

    /// Exchange an authorization code for an access/refresh token pair using
    /// **PKCE** (public client, no `client_secret`).
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<TokenResponse, String> {
        let url = format!("{}/oauth/token", self.base_url);
        let client = crate::http::client::shared();

        let resp = client
            .post(&url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", &self.client_id),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .map_err(|e| format!("oauth token request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            debug!(status = %status, body = %body, "oauth_token_exchange_failed");
            return Err(format!("oauth token exchange failed: {status}"));
        }

        let token: TokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse token response: {e}"))?;

        info!("oauth_token_exchanged");
        Ok(token)
    }

    /// Exchange a refresh token for a fresh access/refresh token pair (public
    /// client, no secret). Used to keep the account premium fresh past the
    /// access-token expiry.
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<TokenResponse, String> {
        let url = format!("{}/oauth/token", self.base_url);
        let client = crate::http::client::shared();

        let resp = client
            .post(&url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &self.client_id),
            ])
            .send()
            .await
            .map_err(|e| format!("oauth refresh request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            debug!(status = %status, "oauth_refresh_failed");
            return Err(format!("oauth refresh failed: {status}"));
        }

        resp.json()
            .await
            .map_err(|e| format!("failed to parse refresh response: {e}"))
    }

    /// Relit le profil du compte lié sur mozaiklabs — `premium`, la date de fin
    /// d'abonnement, l'ordre Qobuz et surtout `modules`, les droits de MODULE
    /// payants.
    ///
    /// 🔴 Passe par [`rate_limit::appeler`], le chemin borné unique (CLD-2),
    /// exactement comme `bio_sync`, la synchro de bibliothèque, la télémétrie
    /// et la revalidation de licence. Deux mécanismes de recul divergents
    /// seraient pires que le défaut d'origine : ici la portée déjà retenue ne
    /// repart pas, et un 429 mémorise son échéance avant d'être rendu.
    ///
    /// Avant, cette fonction rendait `Err("user profile fetch failed: 429 Too
    /// Many Requests")` — un throttle d'une minute remontait tel quel jusqu'à
    /// l'écran d'un testeur, et la relecture du profil était comptée comme un
    /// échec du compte.
    pub async fn get_user(&self, settings: &SettingsRepo, access_token: &str) -> ProfilCloud {
        let url = format!("{}/api/v1/user", self.base_url);
        let client = crate::http::client::shared();

        match rate_limit::appeler(
            settings,
            CloudScope::UserProfile,
            client.get(&url).bearer_auth(access_token),
        )
        .await
        {
            // La portée est encore retenue par un 429 précédent : l'appel n'est
            // même pas parti. Le délai restant est déjà connu.
            AppelCloud::Retenu(backoff) => {
                debug!(
                    scope = backoff.scope,
                    until_epoch = backoff.until_epoch,
                    retry_after_seconds = backoff.retry_after_seconds,
                    "sso_user_profile_deferred_rate_limit"
                );
                ProfilCloud::Differe {
                    retry_after_seconds: backoff.retry_after_seconds,
                }
            }
            AppelCloud::Reponse(resp)
                if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS =>
            {
                // `appeler` a déjà persisté l'échéance quand l'en-tête la
                // donne ; on relit la même source pour DIRE le délai, avec la
                // fenêtre mesurée en repli plutôt qu'un zéro trompeur.
                let retry_after_seconds = rate_limit::retry_after_secs(resp.headers())
                    .unwrap_or(FENETRE_THROTTLE_DEFAUT_S);
                warn!(retry_after_seconds, "sso_user_profile_rate_limited");
                ProfilCloud::Differe {
                    retry_after_seconds,
                }
            }
            AppelCloud::Reponse(resp) if !resp.status().is_success() => {
                let status = resp.status();
                debug!(status = %status, "sso_user_profile_rejected");
                ProfilCloud::Echec(format!("user profile fetch failed: {status}"))
            }
            AppelCloud::Reponse(resp) => match resp.json::<CloudUser>().await {
                Ok(user) => ProfilCloud::Profil(Box::new(user)),
                Err(e) => ProfilCloud::Echec(format!("failed to parse user profile: {e}")),
            },
            AppelCloud::Erreur(e) => {
                ProfilCloud::Echec(format!("user profile request failed: {e}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PKCE helpers (RFC 7636) — no external crypto deps, mirrors the Tidal flow.
// ---------------------------------------------------------------------------

/// Generate a cryptographically random code verifier (RFC 7636 §4.1):
/// 43-128 characters from the unreserved set `[A-Z a-z 0-9 - . _ ~]`.
///
/// Entropy source: three v4 UUIDs (122 random bits each → ~366 bits total),
/// mapped onto the unreserved alphabet. 3 × 16 bytes = 48 chars, always ≥ 43.
fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut result = String::with_capacity(48);
    for _ in 0..3 {
        for &b in uuid::Uuid::new_v4().as_bytes() {
            if result.len() >= 128 {
                break;
            }
            result.push(CHARSET[(b as usize) % CHARSET.len()] as char);
        }
    }
    result
}

/// Compute the S256 code challenge: `base64url(sha256(verifier))`, no padding.
fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url_encode(&hasher.finalize())
}

/// Generate a random CSRF `state` value (URL-safe, no separators).
fn generate_state() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// URL-safe base64 encoding without padding (RFC 4648 §5), used for the S256
/// challenge.
fn base64url_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        buf = (buf << 8) | byte as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            output.push(TABLE[((buf >> bits) & 0x3F) as usize] as char);
        }
    }
    if bits > 0 {
        buf <<= 6 - bits;
        output.push(TABLE[(buf & 0x3F) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_user_parses_without_qobuz_proxy_first() {
        // Retro-compat: older servers omit the field → direct-first.
        let json = r#"{
            "id": 1,
            "email": "a@b.fr",
            "display_name": "A",
            "is_admin": false,
            "avatar_url": null
        }"#;
        let user: CloudUser = serde_json::from_str(json).unwrap();
        assert!(!user.qobuz_proxy_first);
        assert!(!user.premium);
        assert!(user.modules.is_empty());
    }

    #[test]
    fn cloud_user_parses_with_modules() {
        let json = r#"{
            "id": 1,
            "email": "a@b.fr",
            "display_name": "A",
            "is_admin": false,
            "avatar_url": null,
            "premium": false,
            "modules": ["diretta"]
        }"#;
        let user: CloudUser = serde_json::from_str(json).unwrap();
        // A module is its own SKU: owning one does not require premium.
        assert!(!user.premium);
        assert_eq!(user.modules, vec!["diretta".to_string()]);
    }

    #[test]
    fn cloud_user_parses_with_qobuz_proxy_first() {
        let json = r#"{
            "id": 1,
            "email": "a@b.fr",
            "display_name": "A",
            "is_admin": false,
            "avatar_url": null,
            "premium": true,
            "qobuz_proxy_first": true
        }"#;
        let user: CloudUser = serde_json::from_str(json).unwrap();
        assert!(user.qobuz_proxy_first);
    }

    #[test]
    fn authorize_url_format() {
        let auth = MozaikAuth::new("my-client".into(), None);
        let url = auth.authorize_url("http://127.0.0.1:8888/auth/callback", "chal", "st");
        assert!(url.starts_with("https://mozaiklabs.fr/oauth/authorize"));
        assert!(url.contains("client_id=my-client"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn custom_base_url() {
        let auth = MozaikAuth::new("test".into(), Some("http://localhost:3000/"));
        let url = auth.authorize_url("http://127.0.0.1:8888/cb", "chal", "st");
        assert!(url.starts_with("http://localhost:3000/oauth/authorize"));
    }

    #[test]
    fn authorize_url_carries_pkce_params() {
        let auth = MozaikAuth::new("cid".into(), None);
        let pkce = PkceSession::generate();
        let url = auth.authorize_url("http://127.0.0.1:9000/cb", &pkce.challenge, &pkce.state);
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(url.contains(&format!("state={}", pkce.state)));
        // No client secret ever leaks into the public authorize request.
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn verifier_length_and_charset() {
        let v = generate_code_verifier();
        assert!(
            (43..=128).contains(&v.len()),
            "verifier length {} out of RFC range",
            v.len()
        );
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')),
            "verifier contains a non-unreserved char"
        );
    }

    #[test]
    fn verifier_is_random_each_time() {
        assert_ne!(generate_code_verifier(), generate_code_verifier());
    }

    #[test]
    fn challenge_is_deterministic_s256() {
        // Known RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = generate_code_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        // No base64 padding, url-safe alphabet only.
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+') && !challenge.contains('/'));
    }

    #[test]
    fn pkce_session_challenge_matches_verifier() {
        let pkce = PkceSession::generate();
        assert_eq!(pkce.challenge, generate_code_challenge(&pkce.verifier));
        assert!(!pkce.state.is_empty());
    }

    #[test]
    fn base64url_encode_known_vector() {
        // "Man" -> "TWFu" in both standard and url-safe base64.
        assert_eq!(base64url_encode(b"Man"), "TWFu");
        // Single byte 0xFF -> "_w" (url-safe: 62='-', 63='_').
        assert_eq!(base64url_encode(&[0xFF]), "_w");
    }
}

#[cfg(test)]
mod tests_profil_borne {
    use super::*;
    use crate::cloud::rate_limit::{CloudScope, active};
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn settings() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(Arc::new(db))
    }

    const PROFIL_AVEC_MODULE: &str = r#"{
        "id": 7,
        "email": "ludovic@exemple.test",
        "display_name": "Ludovic",
        "is_admin": false,
        "premium": true,
        "modules": ["diretta"]
    }"#;

    /// Monte un `/api/v1/user` local qui rend `reponses[n]` au n-ieme appel
    /// (le dernier est repete), et compte les appels REELLEMENT recus.
    async fn banc(
        reponses: Vec<(u16, Option<&'static str>, &'static str)>,
    ) -> (String, Arc<AtomicU64>) {
        use axum::http::{HeaderValue, StatusCode, header};
        let appels = Arc::new(AtomicU64::new(0));
        let compteur = appels.clone();
        let app = axum::Router::new().route(
            "/api/v1/user",
            axum::routing::get(move || {
                let compteur = compteur.clone();
                let reponses = reponses.clone();
                async move {
                    let n = compteur.fetch_add(1, Ordering::SeqCst) as usize;
                    let (code, retry_after, corps) = reponses[n.min(reponses.len() - 1)];
                    let mut reponse = axum::response::Response::new(axum::body::Body::from(corps));
                    *reponse.status_mut() = StatusCode::from_u16(code).unwrap();
                    reponse.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    if let Some(secs) = retry_after {
                        reponse
                            .headers_mut()
                            .insert(header::RETRY_AFTER, HeaderValue::from_static(secs));
                    }
                    reponse
                }
            }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("port libre");
        let adresse = ecoute.local_addr().expect("adresse locale");
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });
        (format!("http://{adresse}"), appels)
    }

    /// PREMIER SENS — un 429 est un report, pas un verdict.
    ///
    /// Avant, `get_user` rendait `Err("user profile fetch failed: 429 Too Many
    /// Requests")` : indistinguable d'un 401, et l'appelant ne pouvait que le
    /// compter comme un echec du compte. Ici : verdict `Differe`, `Retry-After`
    /// honore, et l'echeance memorisee par le chemin borne COMMUN — donc le
    /// second appel ne part meme pas sur le reseau.
    #[tokio::test]
    async fn un_429_est_differe_honore_son_retry_after_et_retient_la_portee() {
        let settings = settings();
        let (base, appels) = banc(vec![(429, Some("42"), "{}")]).await;
        let auth = MozaikAuth::new(DEFAULT_CLIENT_ID.into(), Some(&base));

        match auth.get_user(&settings, "jeton").await {
            ProfilCloud::Differe {
                retry_after_seconds,
            } => assert_eq!(
                retry_after_seconds, 42,
                "le `Retry-After` du serveur doit etre rendu tel quel"
            ),
            autre => panic!("un 429 doit etre un report, pas un echec : {autre:?}"),
        }

        let retenue = active(&settings, CloudScope::UserProfile)
            .expect("le 429 doit etre memorise par la portee commune (CLD-2)");
        assert_eq!(retenue.scope, "user_profile");

        // La portee est retenue : le second appel ne doit PAS partir.
        match auth.get_user(&settings, "jeton").await {
            ProfilCloud::Differe { .. } => {}
            autre => panic!("une portee retenue ne repart pas : {autre:?}"),
        }
        assert_eq!(
            appels.load(Ordering::SeqCst),
            1,
            "le second appel est parti alors que la portee etait retenue"
        );
    }

    /// Sans en-tete exploitable, la persistance n'invente toujours RIEN (c'est
    /// l'invariant de `defer_from_headers`) — mais l'utilisateur recoit quand
    /// meme un delai a annoncer : la fenetre mesuree sur l'API de production,
    /// et non un « 0 » qui invite a marteler la porte.
    #[tokio::test]
    async fn un_429_sans_entete_annonce_la_fenetre_par_defaut_sans_rien_persister() {
        let settings = settings();
        let (base, _) = banc(vec![(429, None, "{}")]).await;
        let auth = MozaikAuth::new(DEFAULT_CLIENT_ID.into(), Some(&base));

        match auth.get_user(&settings, "jeton").await {
            ProfilCloud::Differe {
                retry_after_seconds,
            } => assert_eq!(retry_after_seconds, FENETRE_THROTTLE_DEFAUT_S),
            autre => panic!("un 429 reste un report meme sans en-tete : {autre:?}"),
        }
        assert!(
            active(&settings, CloudScope::UserProfile).is_none(),
            "aucun delai ne doit etre invente dans la persistance"
        );
    }

    /// SECOND SENS — sans lui, la correction aurait simplement eteint l'alarme.
    ///
    /// Un 401 (jeton mort) et un 500 restent des ECHECS : ils ne doivent
    /// surtout pas se deguiser en report, sinon un compte revoque garderait ses
    /// droits pour toujours et le battement ne tenterait plus jamais de
    /// rafraichir son jeton.
    #[tokio::test]
    async fn un_401_et_un_500_restent_des_echecs() {
        for (code, attendu) in [(401u16, "401"), (500u16, "500")] {
            let settings = settings();
            let (base, _) = banc(vec![(code, None, "{}")]).await;
            let auth = MozaikAuth::new(DEFAULT_CLIENT_ID.into(), Some(&base));

            match auth.get_user(&settings, "jeton").await {
                ProfilCloud::Echec(motif) => assert!(
                    motif.contains(attendu),
                    "le motif doit porter le statut {attendu} : {motif}"
                ),
                autre => panic!("un {code} doit rester un echec : {autre:?}"),
            }
            assert!(
                active(&settings, CloudScope::UserProfile).is_none(),
                "un {code} ne doit RIEN retenir : le prochain cycle doit pouvoir \
                 rafraichir le jeton tout de suite"
            );
        }
    }

    /// Un 200 rapporte bien le profil ET ses droits de MODULE : la correction
    /// ne doit pas avoir casse le chemin nominal.
    #[tokio::test]
    async fn un_profil_lisible_rapporte_ses_modules() {
        let settings = settings();
        let (base, _) = banc(vec![(200, None, PROFIL_AVEC_MODULE)]).await;
        let auth = MozaikAuth::new(DEFAULT_CLIENT_ID.into(), Some(&base));

        let user = auth
            .get_user(&settings, "jeton")
            .await
            .profil()
            .expect("un 200 doit rendre le profil");
        assert_eq!(user.modules, vec!["diretta".to_string()]);
        assert!(user.premium);
    }
}
