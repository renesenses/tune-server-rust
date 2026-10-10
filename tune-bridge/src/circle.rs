//! Écoute chez un contact, Tune Circle étape T4 (#5327).
//!
//! `GET /stream/circle/{ticket}` : un auditeur écoute UNE piste de la
//! bibliothèque d'un contact, servie par le serveur Tune de ce contact.
//!
//! ```text
//! auditeur ── GET /stream/circle/{billet} ──▶ pont ── POST /api/v1/circle/listen/verify ──▶ mozaiklabs
//!                                              │         (jeton de SERVICE du pont)
//!                                              │◀── { ok, server_id, track_id } | 404
//!                                              └── relay.circle_stream_request { id, track_id, range } ──▶ serveur du contact
//! ```
//!
//! Ce que la route admet, et rien d'autre :
//!
//! * **Le billet seul.** Il est délivré par le cloud, court, lié à l'auditeur,
//!   au serveur et à la piste. La route ne lit AUCUN autre moyen
//!   d'authentification : ni en-tête, ni paramètre de requête. Le cloud juge
//!   le droit ; le pont ne fait que lui demander.
//! * **Revérifié à chaque requête HTTP**, sans cache favorable : chaque
//!   `Range` repart au cloud. Une révocation, un retrait du cercle, un partage
//!   coupé ou la fin du Premium valent donc à la requête suivante (décision du
//!   28/09 : le tampon déjà reçu finit de jouer).
//! * **Un seul message vers le serveur**, `relay.circle_stream_request`, qui
//!   ne porte qu'un `track_id` numérique. Jamais `relay.request`, jamais
//!   `relay.stream_request` : le serveur du contact ne peut être amené, par
//!   cette route, qu'à lire l'audio d'une piste.
//!
//! Réponses propres à la route (corps JSON `{ "code" }`) :
//!
//! | Cas | Statut | `code` |
//! |---|---|---|
//! | interrupteur `TUNE_BRIDGE_CIRCLE_LISTEN` fermé (le défaut), sans appel au cloud | 404 | `not_found` |
//! | billet refusé par le cloud, illisible, ou réponse du cloud incohérente | 404 | `not_found` |
//! | serveur du contact non connecté au pont | 503 | `owner_offline` |
//! | cloud injoignable, en panne, ou pont sans jeton de service | 503 | `cloud_unavailable` |
//!
//! Sinon, le statut et les en-têtes audio du serveur du contact (200, 206,
//! 404 pour une piste inconnue, 416…).
//!
//! Journal : ni billet, ni identité. Le `server_id` et le `track_id` seulement,
//! avec le statut et la durée de la vérification.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use crate::state::{CorpsRelaye, PendingResponse, RelayState};

/// Code d'un billet refusé (ou de tout ce qui n'en est pas un).
pub const CODE_REFUSE: &str = "not_found";
/// Code du serveur du contact absent du pont.
pub const CODE_PROPRIETAIRE_ABSENT: &str = "owner_offline";
/// Code d'une vérification impossible.
pub const CODE_CLOUD_INDISPONIBLE: &str = "cloud_unavailable";

/// Type du message émis vers le serveur du contact.
pub const MESSAGE_FLUX_DE_CERCLE: &str = "relay.circle_stream_request";

/// L'interrupteur de l'écoute chez un contact, FERMÉ par défaut (décision
/// produit du 10/10). Indépendant du jeton de service : poser
/// [`JETON_DE_VERIFICATION_ENV`] (pour le contrôle de licence, par exemple)
/// ne rouvre rien. Seules les valeurs `1`, `true`, `on` ou `yes` l'ouvrent.
/// Fermé, `/stream/circle/{ticket}` rend 404 `not_found` sans appeler le
/// cloud.
pub const INTERRUPTEUR_ENV: &str = "TUNE_BRIDGE_CIRCLE_LISTEN";

/// Jeton de service PROPRE au relais pour `POST /listen/verify`
/// (site-mozaiklabs#237), distinct de `TUNE_CLOUD_SERVICE_TOKEN`.
pub const JETON_DE_VERIFICATION_ENV: &str = "TUNE_BRIDGE_SERVICE_TOKEN";

/// Un billet : 64 caractères hexadécimaux minuscules (256 bits,
/// site-mozaiklabs#237). Toute autre forme est refusée sans appeler le cloud.
const LONGUEUR_DU_BILLET: usize = 64;
/// Un `track_id` est un entier positif : 19 chiffres au plus (i64).
const CHIFFRES_MAX_DU_TRACK_ID: usize = 19;
/// Un `Range` plus long n'est pas un intervalle d'octets légitime.
const LONGUEUR_MAX_DU_RANGE: usize = 128;

/// Borne de la vérification : elle est sur le chemin de CHAQUE requête audio.
const DELAI_DE_VERIFICATION: Duration = Duration::from_secs(5);

/// Ce que le cloud dit d'un billet.
#[derive(Debug, Clone, PartialEq)]
pub enum Verification {
    /// Billet valide pour cette piste de ce serveur.
    Accepte { server_id: String, track_id: String },
    /// Billet refusé : expiré, révoqué, inconnu, d'un autre auditeur…
    Refuse,
    /// Le cloud n'a pas pu juger (panne, délai, jeton de service absent ou
    /// refusé). Jamais traité comme un accord.
    Indisponible,
}

/// Le vérificateur de billets. Aucun cache : chaque requête interroge le cloud.
pub struct Billets {
    client: reqwest::Client,
    base: String,
    jeton: Option<String>,
    /// [`INTERRUPTEUR_ENV`] : `false` tant qu'on ne l'ouvre pas explicitement.
    ouverte: bool,
}

/// `1`, `true`, `on`, `yes` (sans casse) : ouvert. Tout le reste, absence
/// comprise : fermé.
fn interrupteur_ouvert(valeur: Option<&str>) -> bool {
    valeur.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

impl Billets {
    /// Même base que la vérification d'éligibilité (`crate::licence`), mais
    /// un jeton de service à part : [`JETON_DE_VERIFICATION_ENV`].
    pub fn depuis_environnement() -> Arc<Self> {
        let base = std::env::var(crate::licence::CLOUD_BASE_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://mozaiklabs.fr".to_string());
        let jeton = std::env::var(JETON_DE_VERIFICATION_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        if jeton.is_none() {
            // À l'inverse de l'éligibilité, AUCUN repli permissif : sans jeton
            // de service, aucune écoute de contact n'est servie.
            warn!(
                "circle_ecoute_fermee — {} absent : aucune ecoute de contact ne sera servie",
                JETON_DE_VERIFICATION_ENV
            );
        }
        let ouverte = interrupteur_ouvert(std::env::var(INTERRUPTEUR_ENV).ok().as_deref());
        if !ouverte {
            warn!(
                "circle_ecoute_fermee — {} n'est pas ouvert : aucune ecoute de contact ne sera servie",
                INTERRUPTEUR_ENV
            );
        }
        Arc::new(Self::nouveau(&base, jeton).avec_ecoute(ouverte))
    }

    pub fn nouveau(base: &str, jeton: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(DELAI_DE_VERIFICATION)
                .build()
                .unwrap_or_default(),
            base: base.trim_end_matches('/').to_string(),
            jeton,
            ouverte: false,
        }
    }

    /// Ouvre (ou ferme) l'écoute de contact. [`Billets::nouveau`] la laisse
    /// fermée.
    pub fn avec_ecoute(mut self, ouverte: bool) -> Self {
        self.ouverte = ouverte;
        self
    }

    /// L'écoute de contact est-elle ouverte sur ce pont ?
    pub fn ecoute_ouverte(&self) -> bool {
        self.ouverte
    }

    /// `POST {base}/api/v1/circle/listen/verify` `{ "ticket" }`, avec le jeton
    /// de service du pont.
    pub async fn verifier(&self, billet: &str) -> Verification {
        let Some(jeton) = self.jeton.as_deref() else {
            return Verification::Indisponible;
        };
        let url = format!("{}/api/v1/circle/listen/verify", self.base);
        let envoi = self
            .client
            .post(&url)
            .bearer_auth(jeton)
            .header(header::ACCEPT, "application/json")
            .json(&serde_json::json!({ "ticket": billet }))
            .send()
            .await;
        let reponse = match envoi {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e.without_url(), "circle_verification_cloud_injoignable");
                return Verification::Indisponible;
            }
        };
        let statut = reponse.status();
        if statut.is_success() {
            let corps: serde_json::Value = reponse.json().await.unwrap_or_default();
            return lire_l_accord(&corps).unwrap_or_else(|| {
                // Un 200 qui ne dit pas clairement « oui, ce serveur, cette
                // piste » n'ouvre rien.
                warn!("circle_verification_reponse_incoherente");
                Verification::Refuse
            });
        }
        match statut.as_u16() {
            // Le verdict du cloud sur le billet lui-même.
            400 | 404 | 410 | 422 => Verification::Refuse,
            // 401/403 : le jeton de service est refusé — une erreur de
            // déploiement, pas un billet révoqué. 429, 5xx : une panne.
            autre => {
                warn!(statut = autre, "circle_verification_impossible");
                Verification::Indisponible
            }
        }
    }
}

/// `{ "ok": true, "server_id", "track_id" }` → l'accord, ou `None`.
///
/// `track_id` est accepté en nombre ou en chaîne, mais toujours réduit à des
/// chiffres : c'est lui qui voyage jusqu'au serveur du contact.
fn lire_l_accord(corps: &serde_json::Value) -> Option<Verification> {
    if corps.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let server_id = corps
        .get("server_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let track_id = match corps.get("track_id")? {
        serde_json::Value::Number(n) => n.as_u64()?.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => return None,
    };
    if !track_id_valide(&track_id) {
        return None;
    }
    Some(Verification::Accepte {
        server_id,
        track_id,
    })
}

/// Des chiffres ASCII, et rien d'autre : ni signe, ni espace, ni `/`, ni `.`.
pub fn track_id_valide(track_id: &str) -> bool {
    !track_id.is_empty()
        && track_id.len() <= CHIFFRES_MAX_DU_TRACK_ID
        && track_id.bytes().all(|b| b.is_ascii_digit())
        && track_id.parse::<i64>().is_ok_and(|n| n > 0)
}

/// Un billet a la forme que le cloud délivre : 64 hexadécimaux minuscules.
fn billet_plausible(billet: &str) -> bool {
    billet.len() == LONGUEUR_DU_BILLET
        && billet
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Le `Range` de l'auditeur, seulement s'il a la forme d'un intervalle d'octets.
fn range_de_l_auditeur(headers: &HeaderMap) -> Option<String> {
    let brut = headers.get(header::RANGE)?.to_str().ok()?.trim();
    let ok = brut.len() <= LONGUEUR_MAX_DU_RANGE
        && brut.starts_with("bytes=")
        && brut[6..]
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b',' || b == b' ');
    ok.then(|| brut.to_string())
}

fn refus(statut: StatusCode, code: &str) -> Response {
    let mut r = (statut, axum::Json(serde_json::json!({ "code": code }))).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// En-têtes du serveur du contact qui passent jusqu'à l'auditeur : ceux d'un
/// flux audio, rien d'autre.
const ENTETES_TRANSMIS: [&str; 4] = [
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
];

/// `GET /stream/circle/{ticket}`.
pub async fn flux_de_cercle(
    State(state): State<Arc<RelayState>>,
    Path(billet): Path<String>,
    headers: HeaderMap,
) -> Response {
    let debut = Instant::now();
    // Interrupteur fermé (le défaut) : la route n'existe pas, et le cloud
    // n'est pas appelé. Indépendant du jeton de service.
    if !state.billets.ecoute_ouverte() {
        return refus(StatusCode::NOT_FOUND, CODE_REFUSE);
    }
    if !billet_plausible(&billet) {
        return refus(StatusCode::NOT_FOUND, CODE_REFUSE);
    }

    let (server_id, track_id) = match state.billets.verifier(&billet).await {
        Verification::Accepte {
            server_id,
            track_id,
        } => (server_id, track_id),
        Verification::Refuse => {
            info!(
                duree_ms = debut.elapsed().as_millis() as u64,
                "circle_billet_refuse"
            );
            return refus(StatusCode::NOT_FOUND, CODE_REFUSE);
        }
        Verification::Indisponible => {
            return refus(StatusCode::SERVICE_UNAVAILABLE, CODE_CLOUD_INDISPONIBLE);
        }
    };

    // Les deux `Arc` sortent du garde de la DashMap avant toute attente.
    let (ws_tx, pending) = match state.servers.get(&server_id) {
        Some(conn) => (conn.ws_tx.clone(), conn.pending.clone()),
        None => {
            info!(server_id = %server_id, "circle_proprietaire_absent");
            return refus(StatusCode::SERVICE_UNAVAILABLE, CODE_PROPRIETAIRE_ABSENT);
        }
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let message = serde_json::json!({
        "type": MESSAGE_FLUX_DE_CERCLE,
        "id": request_id,
        "track_id": track_id,
        "range": range_de_l_auditeur(&headers),
    });

    let (tx, rx) = tokio::sync::oneshot::channel::<PendingResponse>();
    pending.lock().await.insert(request_id.clone(), tx);
    if ws_tx.send(message.to_string()).await.is_err() {
        pending.lock().await.remove(&request_id);
        return refus(StatusCode::SERVICE_UNAVAILABLE, CODE_PROPRIETAIRE_ABSENT);
    }
    info!(
        server_id = %server_id,
        track_id = %track_id,
        verification_ms = debut.elapsed().as_millis() as u64,
        "circle_flux_demande"
    );

    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(resp)) => reponse_a_l_auditeur(resp),
        Ok(Err(_)) => StatusCode::BAD_GATEWAY.into_response(),
        Err(_) => {
            pending.lock().await.remove(&request_id);
            warn!(server_id = %server_id, "circle_flux_delai_depasse");
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

/// La réponse du serveur du contact, réduite aux en-têtes d'un flux audio.
fn reponse_a_l_auditeur(mut resp: PendingResponse) -> Response {
    resp.headers
        .retain(|nom, _| ENTETES_TRANSMIS.contains(&nom.to_ascii_lowercase().as_str()));
    if resp.status >= 400 {
        // Un refus du serveur (piste inconnue, intervalle impossible…) : le
        // statut seul, sans corps.
        resp.body = CorpsRelaye::Entier(None);
        resp.headers.clear();
    }
    let mut r = crate::stream_proxy::reponse_relayee(resp);
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::Router;
    use axum::routing::post;
    use tokio::sync::mpsc;

    const PATIENCE: Duration = Duration::from_secs(5);
    const JETON_DE_SERVICE: &str = "service-SECRET-5327";
    /// Un jeton de pont qui a la FORME d'un billet : la route ne doit pas
    /// l'accepter pour autant.
    const JETON_DE_PONT: &str = "5327000000000000000000000000000000000000000000000000000000000b0b";
    /// Un billet bien formé.
    const BILLET: &str = "5327aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Un faux mozaiklabs : `POST /api/v1/circle/listen/verify` rend, dans
    /// l'ordre, les verdicts préparés (puis 404), et note chaque appel.
    #[derive(Default)]
    struct FauxCloud {
        verdicts: Vec<(u16, serde_json::Value)>,
        /// (en-tête Authorization, corps reçu)
        appels: Vec<(Option<String>, serde_json::Value)>,
    }

    type Cloud = Arc<Mutex<FauxCloud>>;

    async fn verifier_chez_le_faux(
        State(cloud): State<Cloud>,
        headers: HeaderMap,
        axum::Json(corps): axum::Json<serde_json::Value>,
    ) -> Response {
        let mut c = cloud.lock().unwrap();
        c.appels.push((
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            corps,
        ));
        let (statut, corps) = if c.verdicts.is_empty() {
            (404, serde_json::json!({ "error": "not_found" }))
        } else {
            c.verdicts.remove(0)
        };
        (StatusCode::from_u16(statut).unwrap(), axum::Json(corps)).into_response()
    }

    async fn faux_cloud(verdicts: Vec<(u16, serde_json::Value)>) -> (String, Cloud) {
        let cloud: Cloud = Arc::new(Mutex::new(FauxCloud {
            verdicts,
            appels: Vec::new(),
        }));
        let app = Router::new()
            .route("/api/v1/circle/listen/verify", post(verifier_chez_le_faux))
            .with_state(cloud.clone());
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        (format!("http://{adresse}"), cloud)
    }

    fn accord(server_id: &str, track_id: serde_json::Value) -> (u16, serde_json::Value) {
        (
            200,
            serde_json::json!({ "ok": true, "server_id": server_id, "track_id": track_id }),
        )
    }

    fn pont(base: &str, jeton: Option<&str>) -> Arc<RelayState> {
        Arc::new(RelayState {
            billets: Arc::new(Billets::nouveau(base, jeton.map(String::from)).avec_ecoute(true)),
            ..RelayState::new()
        })
    }

    /// Un pont AVEC jeton de service mais interrupteur fermé.
    fn pont_ferme(base: &str) -> Arc<RelayState> {
        Arc::new(RelayState {
            billets: Arc::new(Billets::nouveau(base, Some(JETON_DE_SERVICE.into()))),
            ..RelayState::new()
        })
    }

    /// Enregistre le serveur `srv`, et rend ce que le pont lui envoie.
    fn serveur_connecte(state: &RelayState) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel::<String>(16);
        state
            .register_server("srv".into(), "Salon".into(), JETON_DE_PONT.into(), tx)
            .unwrap();
        rx
    }

    /// Le serveur du contact, simulé : lit UN message, et répond comme le fait
    /// `tune-core/src/cloud/relay.rs` (en-tête du flux, un morceau, la fin).
    fn serveur_qui_repond(
        state: Arc<RelayState>,
        mut rx: mpsc::Receiver<String>,
    ) -> tokio::task::JoinHandle<serde_json::Value> {
        tokio::spawn(async move {
            let brut = tokio::time::timeout(PATIENCE, rx.recv())
                .await
                .expect("le pont n'a rien envoye au serveur")
                .unwrap();
            let msg: serde_json::Value = serde_json::from_str(&brut).unwrap();
            let id = msg["id"].as_str().unwrap().to_string();
            let debut = serde_json::json!({
                "type": "relay.stream_start",
                "id": id,
                "status": 206,
                "headers": {
                    "content-type": "audio/flac",
                    "content-range": "bytes 0-3/1000",
                    "accept-ranges": "bytes",
                    "x-interne": "ne-doit-pas-sortir",
                },
                "content_length": 4,
            });
            crate::ws_server::handle_server_message(&state, "srv", &debut.to_string()).await;
            crate::ws_server::handle_server_message(
                &state,
                "srv",
                &format!("BINARY:{id}:ZkxhQw=="),
            )
            .await;
            crate::ws_server::handle_server_message(
                &state,
                "srv",
                &serde_json::json!({"type": "relay.stream_end", "id": id}).to_string(),
            )
            .await;
            msg
        })
    }

    async fn appeler(state: &Arc<RelayState>, billet: &str, headers: HeaderMap) -> Response {
        tokio::time::timeout(
            PATIENCE,
            flux_de_cercle(State(state.clone()), Path(billet.to_string()), headers),
        )
        .await
        .expect("la route ne repond pas")
    }

    /// Appelle la route en surveillant le serveur du contact : un message qui
    /// lui parvient fait échouer le témoin sur-le-champ, en le nommant.
    async fn appeler_sans_emission(
        state: &Arc<RelayState>,
        billet: &str,
        headers: HeaderMap,
        rx: &mut mpsc::Receiver<String>,
    ) -> Response {
        let route = appeler(state, billet, headers);
        tokio::pin!(route);
        tokio::select! {
            r = &mut route => r,
            Some(msg) = rx.recv() => panic!("un message est parti vers le serveur du contact : {msg}"),
        }
    }

    async fn corps(r: Response) -> Vec<u8> {
        axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn avec_range(range: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_str(range).unwrap());
        h
    }

    fn rien_n_est_parti(rx: &mut mpsc::Receiver<String>) {
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "un message est parti vers le serveur du contact"
        );
    }

    /// Billet valide : `relay.circle_stream_request` part avec le bon
    /// `track_id` et le `Range`, jamais `relay.request` ; l'audio revient.
    #[tokio::test]
    async fn un_billet_valide_demande_le_flux_de_cette_piste_seulement() {
        let (base, cloud) = faux_cloud(vec![accord("srv", serde_json::json!("42"))]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let rx = serveur_connecte(&state);
        let serveur = serveur_qui_repond(state.clone(), rx);

        let r = appeler(&state, BILLET, avec_range("bytes=0-3")).await;
        let msg = serveur.await.unwrap();

        assert_eq!(msg["type"], MESSAGE_FLUX_DE_CERCLE);
        assert_eq!(msg["track_id"], "42");
        assert_eq!(msg["range"], "bytes=0-3");
        assert_eq!(
            msg.as_object().unwrap().len(),
            4,
            "le message ne porte que type, id, track_id, range : {msg}"
        );

        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers()["content-type"], "audio/flac");
        assert_eq!(r.headers()["content-range"], "bytes 0-3/1000");
        assert!(r.headers().get("x-interne").is_none());
        assert_eq!(corps(r).await, b"fLaC".to_vec());

        // Le cloud a reçu le billet, avec le jeton de SERVICE du pont.
        let c = cloud.lock().unwrap();
        assert_eq!(c.appels.len(), 1);
        assert_eq!(
            c.appels[0].0.as_deref(),
            Some(format!("Bearer {JETON_DE_SERVICE}").as_str())
        );
        assert_eq!(c.appels[0].1, serde_json::json!({ "ticket": BILLET }));
    }

    /// Billet refusé : 404, rien n'est émis.
    #[tokio::test]
    async fn un_billet_refuse_rend_404_et_rien_ne_part() {
        let (base, _cloud) = faux_cloud(vec![]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let mut rx = serveur_connecte(&state);

        let r = appeler_sans_emission(&state, BILLET, HeaderMap::new(), &mut rx).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_slice(&corps(r).await).unwrap();
        assert_eq!(v, serde_json::json!({ "code": CODE_REFUSE }));
        rien_n_est_parti(&mut rx);
    }

    /// Serveur du contact absent : 503 `owner_offline`, et non plus un 404 nu.
    #[tokio::test]
    async fn serveur_absent_rend_503_owner_offline() {
        let (base, _cloud) = faux_cloud(vec![accord("srv-eteint", serde_json::json!(42))]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let mut rx = serveur_connecte(&state);

        let r = appeler_sans_emission(&state, BILLET, HeaderMap::new(), &mut rx).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_slice(&corps(r).await).unwrap();
        assert_eq!(v, serde_json::json!({ "code": CODE_PROPRIETAIRE_ABSENT }));
        rien_n_est_parti(&mut rx);
    }

    /// Revérification à CHAQUE requête : le cloud accepte la première, refuse
    /// la seconde (révocation entre deux `Range`) : le pont coupe.
    #[tokio::test]
    async fn chaque_range_est_reverifie_et_une_revocation_coupe_la_suivante() {
        let (base, cloud) = faux_cloud(vec![
            accord("srv", serde_json::json!("42")),
            (404, serde_json::json!({ "error": "not_found" })),
        ])
        .await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let rx = serveur_connecte(&state);
        let serveur = serveur_qui_repond(state.clone(), rx);

        let r1 = appeler(&state, BILLET, avec_range("bytes=0-3")).await;
        assert_eq!(r1.status(), StatusCode::PARTIAL_CONTENT);
        let _ = corps(r1).await;
        let _ = serveur.await.unwrap();

        let mut rx2 = {
            // Le récepteur a été consommé par la tâche : on en rebranche un.
            state.unregister_server("srv");
            serveur_connecte(&state)
        };
        let r2 = appeler_sans_emission(&state, BILLET, avec_range("bytes=4-"), &mut rx2).await;
        assert_eq!(r2.status(), StatusCode::NOT_FOUND);
        rien_n_est_parti(&mut rx2);
        assert_eq!(
            cloud.lock().unwrap().appels.len(),
            2,
            "la seconde requete devait repartir au cloud, sans cache"
        );
    }

    /// Le jeton du pont n'ouvre PAS cette route : ni en billet, ni en
    /// en-tête, ni en paramètre. Seul le cloud dit oui.
    #[tokio::test]
    async fn le_jeton_du_pont_n_ouvre_pas_la_route_de_cercle() {
        let (base, cloud) = faux_cloud(vec![]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let mut rx = serveur_connecte(&state);

        let mut h = HeaderMap::new();
        h.insert("x-bridge-token", HeaderValue::from_static(JETON_DE_PONT));
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("BridgeToken {JETON_DE_PONT}")).unwrap(),
        );
        let r = appeler_sans_emission(&state, JETON_DE_PONT, h, &mut rx).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let octets = corps(r).await;
        assert!(!String::from_utf8_lossy(&octets).contains(JETON_DE_PONT));
        rien_n_est_parti(&mut rx);
        // Le cloud a jugé le billet présenté (et l'a refusé) : le pont n'a
        // pas pris sa connaissance du jeton pour un droit.
        let c = cloud.lock().unwrap();
        assert_eq!(c.appels.len(), 1);
        assert_eq!(
            c.appels[0].0.as_deref(),
            Some(format!("Bearer {JETON_DE_SERVICE}").as_str())
        );
    }

    /// Le jeton du pont ne passe jamais dans une réponse faite à un auditeur,
    /// même quand le serveur du contact renvoie des en-têtes inattendus.
    #[tokio::test]
    async fn le_jeton_du_pont_ne_sort_jamais_vers_l_auditeur() {
        let (base, _cloud) = faux_cloud(vec![accord("srv", serde_json::json!("7"))]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let mut rx = serveur_connecte(&state);
        let s = state.clone();
        let serveur = tokio::spawn(async move {
            let brut = rx.recv().await.unwrap();
            let msg: serde_json::Value = serde_json::from_str(&brut).unwrap();
            let id = msg["id"].as_str().unwrap().to_string();
            let debut = serde_json::json!({
                "type": "relay.stream_start", "id": id, "status": 200,
                "headers": { "content-type": "audio/flac", "x-bridge-token": JETON_DE_PONT,
                             "set-cookie": JETON_DE_PONT },
            });
            crate::ws_server::handle_server_message(&s, "srv", &debut.to_string()).await;
            crate::ws_server::handle_server_message(
                &s,
                "srv",
                &serde_json::json!({"type": "relay.stream_end", "id": id}).to_string(),
            )
            .await;
        });
        let r = appeler(&state, BILLET, HeaderMap::new()).await;
        serveur.await.unwrap();
        for (nom, valeur) in r.headers() {
            assert!(
                !valeur.to_str().unwrap_or("").contains(JETON_DE_PONT),
                "l'en-tete {nom} porte le jeton du pont"
            );
        }
        assert!(!String::from_utf8_lossy(&corps(r).await).contains(JETON_DE_PONT));
    }

    /// Sans jeton de service, la route est FERMÉE (503), et le cloud n'est pas
    /// appelé : aucun repli permissif, à l'inverse de l'éligibilité.
    #[tokio::test]
    async fn sans_jeton_de_service_aucune_ecoute_n_est_servie() {
        let (base, cloud) = faux_cloud(vec![accord("srv", serde_json::json!("42"))]).await;
        let state = pont(&base, None);
        let mut rx = serveur_connecte(&state);

        let r = appeler_sans_emission(&state, BILLET, HeaderMap::new(), &mut rx).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        rien_n_est_parti(&mut rx);
        assert!(cloud.lock().unwrap().appels.is_empty());
    }

    /// Un cloud qui rendrait un `track_id` non numérique ne fait rien partir :
    /// seul un entier voyage jusqu'au serveur du contact.
    #[tokio::test]
    async fn un_track_id_non_numerique_du_cloud_ne_part_pas() {
        for mauvais in [
            serde_json::json!("../1"),
            serde_json::json!("1/../../system"),
            serde_json::json!("-1"),
            serde_json::json!("0"),
            serde_json::json!(""),
            serde_json::json!(" 1"),
            serde_json::json!(1.5),
        ] {
            let (base, _cloud) = faux_cloud(vec![accord("srv", mauvais.clone())]).await;
            let state = pont(&base, Some(JETON_DE_SERVICE));
            let mut rx = serveur_connecte(&state);
            let r = appeler_sans_emission(&state, BILLET, HeaderMap::new(), &mut rx).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "track_id {mauvais}");
            rien_n_est_parti(&mut rx);
        }
    }

    /// Cloud en panne : 503, rien ne part, et ce n'est jamais un accord.
    #[tokio::test]
    async fn cloud_en_panne_rend_503_et_rien_ne_part() {
        let (base, _cloud) = faux_cloud(vec![(500, serde_json::json!({}))]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let mut rx = serveur_connecte(&state);
        let r = appeler_sans_emission(&state, BILLET, HeaderMap::new(), &mut rx).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_slice(&corps(r).await).unwrap();
        assert_eq!(v["code"], CODE_CLOUD_INDISPONIBLE);
        rien_n_est_parti(&mut rx);
    }

    /// Ce qui n'a pas la forme d'un billet est refusé sans même appeler le
    /// cloud.
    #[tokio::test]
    async fn ce_qui_n_a_pas_la_forme_d_un_billet_n_appelle_pas_le_cloud() {
        let (base, cloud) = faux_cloud(vec![accord("srv", serde_json::json!("42"))]).await;
        let state = pont(&base, Some(JETON_DE_SERVICE));
        let mut rx = serveur_connecte(&state);
        let majuscules = BILLET.to_uppercase();
        let court = &BILLET[..63];
        let long = format!("{BILLET}a");
        for mauvais in ["", "..", court, long.as_str(), majuscules.as_str()] {
            let r = appeler_sans_emission(&state, mauvais, HeaderMap::new(), &mut rx).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{mauvais:?}");
        }
        rien_n_est_parti(&mut rx);
        assert!(cloud.lock().unwrap().appels.is_empty());
    }

    /// Décision produit du 10/10 : interrupteur fermé (le défaut), un billet
    /// que le cloud accepterait rend 404 `not_found`, le cloud n'est PAS
    /// appelé et rien ne part vers le serveur du contact — même avec le jeton
    /// de service posé.
    #[tokio::test]
    async fn interrupteur_ferme_rend_404_sans_appeler_le_cloud() {
        let (base, cloud) = faux_cloud(vec![accord("srv", serde_json::json!("42"))]).await;
        let state = pont_ferme(&base);
        let mut rx = serveur_connecte(&state);

        let r = appeler_sans_emission(&state, BILLET, HeaderMap::new(), &mut rx).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_slice(&corps(r).await).unwrap();
        assert_eq!(v["code"], CODE_REFUSE);
        rien_n_est_parti(&mut rx);
        assert!(cloud.lock().unwrap().appels.is_empty());
    }

    #[test]
    fn l_interrupteur_est_ferme_par_defaut_et_ne_s_ouvre_qu_explicitement() {
        assert!(!Billets::nouveau("http://x", Some("j".into())).ecoute_ouverte());
        for ferme in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("off"),
            Some("non"),
        ] {
            assert!(!interrupteur_ouvert(ferme), "{ferme:?}");
        }
        for ouvert in ["1", "true", "TRUE", " on ", "yes"] {
            assert!(interrupteur_ouvert(Some(ouvert)), "{ouvert:?}");
        }
    }

    #[test]
    fn un_range_qui_n_est_pas_un_intervalle_d_octets_ne_part_pas() {
        assert_eq!(
            range_de_l_auditeur(&avec_range("bytes=0-")).as_deref(),
            Some("bytes=0-")
        );
        assert_eq!(range_de_l_auditeur(&avec_range("items=0-1")), None);
        assert_eq!(range_de_l_auditeur(&avec_range("bytes=0-1;x")), None);
        assert_eq!(range_de_l_auditeur(&HeaderMap::new()), None);
    }
}
