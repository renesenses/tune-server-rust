use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;
use tracing::warn;

use crate::state::{Attente, PendingResponse, RelayState};
use crate::trafic::{Categorie, Erreur};

pub async fn proxy_api(
    State(state): State<Arc<RelayState>>,
    Path((server_id, path)): Path<(String, String)>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    let debut = std::time::Instant::now();
    // Validate server exists
    let conn = match state.servers.get(&server_id) {
        Some(c) => c,
        None => {
            state.trafic.erreur(Erreur::ServeurAbsent);
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    // Auth : jeton de pont, sur son PROPRE en-tete.
    //
    // Il vivait dans `Authorization: BridgeToken …`, et cet en-tete etait
    // ensuite retransmis tel quel au serveur. Or le serveur, quand
    // `auth_enabled` vaut true, attend `Authorization: Bearer <jwt>` au meme
    // endroit : les deux ne peuvent pas coexister. Un utilisateur qui protege
    // son serveur — ce que tout acces depuis Internet devrait imposer — se
    // retrouvait avec un relais qui mange l'en-tete dont le serveur a besoin.
    //
    // `X-Bridge-Token` separe les deux. L'ancienne forme reste acceptee : la
    // premiere version de tune-remote l'utilise, et casser un client deja
    // livre pour une question de propriete serait mal echange.
    //
    // A defaut d'en-tete, une LECTURE (GET, HEAD) peut porter le jeton dans
    // l'URL, `?token=` ou `?bridge_token=` : un `<img src>`, un `<a href>` de
    // telechargement ou une navigation n'ont aucun moyen de poser un en-tete
    // — c'est deja la regle de `/stream/relay` pour la balise `<audio>`.
    // Jamais pour une ecriture : un lien piege ne doit pas pouvoir agir.
    let token = extraire_jeton(&headers).or_else(|| {
        matches!(method, Method::GET | Method::HEAD)
            .then(|| jeton_de_requete(uri.query()))
            .flatten()
    });
    // Jeton de session d'ecoute a distance (#6603), ou ancien jeton du pont
    // pendant la coexistence. Voir `RelayState::autoriser`.
    let acces = match state.autoriser(token.as_deref(), &server_id) {
        Ok(a) => a,
        Err(rejet) => return rejet.en_reponse(),
    };
    let token = token.unwrap_or_default();
    // Le jeton ne doit JAMAIS atteindre le serveur (journaux d'acces,
    // historiques) : on retire les parametres qui le portent, et eux seuls.
    let requete = requete_sans_jeton(uri.query(), &token);

    let request_id = uuid::Uuid::new_v4().to_string();

    // Build relay headers (forward relevant ones)
    let mut relay_headers = entetes_a_relayer(&headers);
    if let crate::sessions::Acces::Session(s) = &acces {
        ajouter_appareil_distant(&mut relay_headers, s);
    }

    let body_str = if body.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&body).into_owned())
    };

    let request_msg = serde_json::json!({
        "type": "relay.request",
        "id": request_id,
        "method": method.as_str(),
        "path": match requete {
            Some(q) => format!("/api/v1/{path}?{q}"),
            None => format!("/api/v1/{path}"),
        },
        "headers": relay_headers,
        "body": body_str,
    });

    // Register pending response
    let (tx, rx) = oneshot::channel::<PendingResponse>();
    conn.pending
        .lock()
        .await
        .insert(request_id.clone(), Attente::nouvelle(tx, Categorie::Api));

    // Ce qui entre : le corps de la requete du navigateur. Ce qui sort : le
    // message vers le serveur (#6512).
    let compteurs = conn.trafic.clone();
    state.trafic.entree(&compteurs, Categorie::Api, body.len());
    let message = request_msg.to_string();

    // Send to server
    if conn.ws_tx.send(message.clone()).await.is_err() {
        conn.pending.lock().await.remove(&request_id);
        state.trafic.erreur(Erreur::ServeurAbsent);
        return StatusCode::BAD_GATEWAY.into_response();
    }
    state
        .trafic
        .sortie(&compteurs, Categorie::Api, message.len());

    drop(conn);

    // Wait for response with timeout
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(resp)) => crate::sessions::habiller(
            crate::stream_proxy::reponse_relayee_comptee(
                resp,
                Some(crate::stream_proxy::Comptage {
                    trafic: state.trafic.clone(),
                    serveur: compteurs,
                    categorie: Categorie::Api,
                    cle: path,
                    range: None,
                    debut,
                }),
            ),
            &acces,
        ),
        Ok(Err(_)) => {
            state.trafic.erreur(Erreur::CanalRompu);
            warn!(request_id = %request_id, "response channel dropped");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(_) => {
            if let Some(conn) = state.servers.get(&server_id) {
                conn.pending.lock().await.remove(&request_id);
            }
            state.trafic.erreur(Erreur::Delai);
            warn!(request_id = %request_id, "relay request timeout (30s)");
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

/// En-tetes du navigateur transmis au serveur — liste BLANCHE.
///
/// - `content-type`, `accept` : la negociation de base ;
/// - `authorization` : le `Bearer` du compte Tune (voir plus bas) ;
/// - `range`, `if-range` : lecture partielle ;
/// - `if-none-match`, `if-modified-since`, `cache-control` : les validateurs
///   de cache, sans lesquels chaque pochette repart en entier ;
/// - `x-profile-id` : le profil actif, sous le nom que le serveur lit
///   (`ActiveProfile`, `tune-server/src/routes/active_profile.rs`) et que
///   tous les clients envoient (web, Flutter, iOS). Sans lui, le serveur se
///   rabat sur le profil par defaut, et la garde de `/profiles/{id}/favorites`
///   repond « profile not found » des que le chemin nomme un autre profil
///   (#6479). La liste portait jusqu'ici `x-tune-profile`, un nom qu'aucun
///   client n'envoie et que le serveur ne lit pas : l'en-tete reel etait
///   donc retire en route.
///
/// Ni `cookie` (la session du navigateur vise le domaine du pont, pas le
/// serveur), ni `x-bridge-token` (il ne concerne que le relais).
pub const ENTETES_RELAYES: [&str; 9] = [
    "content-type",
    "accept",
    "authorization",
    "range",
    "if-range",
    "if-none-match",
    "if-modified-since",
    "cache-control",
    "x-profile-id",
];

/// Les en-tetes de la requete du navigateur a transmettre au serveur.
pub fn entetes_a_relayer(headers: &HeaderMap) -> serde_json::Map<String, serde_json::Value> {
    let mut relayes = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let key = name.as_str();
        if !ENTETES_RELAYES.contains(&key) {
            continue;
        }
        let Ok(v) = value.to_str() else { continue };
        // Ne PAS transmettre un `Authorization` qui porte le jeton de pont :
        // il ne concerne que le relais, et le serveur y chercherait un
        // `Bearer`. Un vrai `Bearer` destine au serveur passe, lui, sans y
        // toucher.
        if key == "authorization" && porte_un_jeton_de_pont(v) {
            continue;
        }
        relayes.insert(key.to_string(), serde_json::Value::String(v.to_string()));
    }
    relayes
}

/// En-tetes que le RELAIS pose vers le serveur pour une session ouverte par
/// laissez-passer (#6603) : l'appareil connecte a distance, pour que le
/// serveur l'affiche. Information, pas controle. Hors de
/// [`ENTETES_RELAYES`] : un navigateur ne peut pas les poser lui-meme.
pub const ENTETE_APPAREIL_ID: &str = "x-tune-appareil-distant";
pub const ENTETE_APPAREIL_NOM: &str = "x-tune-appareil-distant-nom";

/// Pose les en-tetes de l'appareil distant. Le nom, choisi par
/// l'utilisateur, est encode en pourcent : un en-tete n'est pas de l'UTF-8.
pub fn ajouter_appareil_distant(
    relayes: &mut serde_json::Map<String, serde_json::Value>,
    session: &crate::sessions::Session,
) {
    relayes.insert(
        ENTETE_APPAREIL_ID.to_string(),
        serde_json::Value::String(session.did.clone()),
    );
    if let Some(nom) = session.nom_appareil() {
        relayes.insert(
            ENTETE_APPAREIL_NOM.to_string(),
            serde_json::Value::String(encoder_pourcent(&nom)),
        );
    }
}

/// Encode en pourcent tout ce qui n'est pas alphanumerique ASCII, `-`, `_`,
/// `.` ou `~`.
pub fn encoder_pourcent(texte: &str) -> String {
    let mut sortie = String::with_capacity(texte.len());
    for o in texte.bytes() {
        if o.is_ascii_alphanumeric() || matches!(o, b'-' | b'_' | b'.' | b'~') {
            sortie.push(o as char);
        } else {
            sortie.push_str(&format!("%{o:02X}"));
        }
    }
    sortie
}

/// Noms des parametres d'URL qui peuvent porter le jeton de pont.
pub const PARAMETRES_DU_JETON: [&str; 2] = ["token", "bridge_token"];

/// Jeton de pont lu dans la chaine de requete (`token=` ou `bridge_token=`).
pub fn jeton_de_requete(requete: Option<&str>) -> Option<String> {
    requete?
        .split('&')
        .filter_map(|paire| paire.split_once('='))
        .filter(|(nom, _)| PARAMETRES_DU_JETON.contains(nom))
        .map(|(_, valeur)| decoder_pourcent(valeur).trim().to_string())
        .find(|t| !t.is_empty())
}

/// La chaine de requete sans les parametres qui portent `jeton`.
///
/// Seuls les parametres dont la VALEUR est le jeton de pont sont retires : un
/// `?token=` qui appartient au serveur passe intact. Les autres paires sont
/// recopiees telles quelles, sans re-encodage. `None` quand il ne reste rien.
pub fn requete_sans_jeton(requete: Option<&str>, jeton: &str) -> Option<String> {
    let reste: Vec<&str> = requete?
        .split('&')
        .filter(|paire| !paire.is_empty())
        .filter(|paire| match paire.split_once('=') {
            Some((nom, valeur)) => {
                !(PARAMETRES_DU_JETON.contains(&nom) && decoder_pourcent(valeur).trim() == jeton)
            }
            None => true,
        })
        .collect();
    (!reste.is_empty()).then(|| reste.join("&"))
}

/// L'URI telle qu'elle peut paraitre dans un journal : la valeur de tout
/// parametre `token` / `bridge_token` est masquee. Le jeton de pont ouvre
/// l'acces complet a un serveur ; il n'a rien a faire dans des traces.
pub fn uri_masquee(uri: &Uri) -> String {
    let Some(requete) = uri.query() else {
        return uri.path().to_string();
    };
    let masquee: Vec<String> = requete
        .split('&')
        .map(|paire| match paire.split_once('=') {
            Some((nom, _)) if PARAMETRES_DU_JETON.contains(&nom) => format!("{nom}=***"),
            _ => paire.to_string(),
        })
        .collect();
    format!("{}?{}", uri.path(), masquee.join("&"))
}

/// Decodage `application/x-www-form-urlencoded` d'une valeur : `+` et `%XX`.
/// Une sequence invalide est gardee telle quelle.
fn decoder_pourcent(valeur: &str) -> String {
    let octets = valeur.as_bytes();
    let mut sortie = Vec::with_capacity(octets.len());
    let mut i = 0;
    while i < octets.len() {
        match octets[i] {
            b'+' => sortie.push(b' '),
            b'%' if i + 2 < octets.len() => {
                let hex = std::str::from_utf8(&octets[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(o) => {
                        sortie.push(o);
                        i += 2;
                    }
                    None => sortie.push(b'%'),
                }
            }
            o => sortie.push(o),
        }
        i += 1;
    }
    String::from_utf8_lossy(&sortie).into_owned()
}

/// En-tete dedie au jeton de pont.
pub const BRIDGE_TOKEN_HEADER: &str = "x-bridge-token";

/// Vrai si cette valeur d'`Authorization` porte un jeton de pont — donc si
/// elle s'adresse au relais et non au serveur.
pub fn porte_un_jeton_de_pont(valeur: &str) -> bool {
    let v = valeur.trim_start();
    v.len() >= 12 && v[..12].eq_ignore_ascii_case("BridgeToken ")
}

/// Jeton presente par le client, quelle que soit la forme.
///
/// `X-Bridge-Token` d'abord ; a defaut l'ancienne forme
/// `Authorization: BridgeToken …`, conservee pour les clients deja livres.
pub fn extraire_jeton(headers: &HeaderMap) -> Option<String> {
    if let Some(t) = headers
        .get(BRIDGE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some(t.to_string());
    }
    let auth = headers.get("authorization")?.to_str().ok()?;
    if !porte_un_jeton_de_pont(auth) {
        return None;
    }
    let t = auth.trim_start()[12..].trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod jeton_de_pont_tests {
    use super::*;
    use axum::http::HeaderValue;

    fn entetes(paires: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in paires {
            // `HeaderName::from_bytes` plutot que `insert(*k, …)` : ce dernier
            // exige un nom 'static, que des &str de test n'ont pas.
            let nom = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
            h.insert(nom, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn len_tete_dedie_est_lu() {
        let h = entetes(&[(BRIDGE_TOKEN_HEADER, "jeton-1")]);
        assert_eq!(extraire_jeton(&h).as_deref(), Some("jeton-1"));
    }

    /// La premiere version de tune-remote envoie l'ancienne forme. La casser
    /// pour une question de proprete serait mal echange.
    #[test]
    fn lancienne_forme_reste_acceptee() {
        let h = entetes(&[("authorization", "BridgeToken jeton-2")]);
        assert_eq!(extraire_jeton(&h).as_deref(), Some("jeton-2"));
    }

    #[test]
    fn len_tete_dedie_prime_sur_lancienne_forme() {
        let h = entetes(&[
            (BRIDGE_TOKEN_HEADER, "neuf"),
            ("authorization", "BridgeToken ancien"),
        ]);
        assert_eq!(extraire_jeton(&h).as_deref(), Some("neuf"));
    }

    /// LE point de ce changement : un `Bearer` s'adresse au SERVEUR, pas au
    /// relais. Le confondre avec un jeton de pont reviendrait a refuser
    /// l'acces a un utilisateur qui a protege son serveur.
    #[test]
    fn un_bearer_nest_pas_un_jeton_de_pont() {
        let h = entetes(&[("authorization", "Bearer eyJhbGciOi.jwt.signature")]);
        assert_eq!(extraire_jeton(&h), None);
        assert!(!porte_un_jeton_de_pont("Bearer eyJhbGciOi.jwt.signature"));
    }

    /// Le serveur attend `Authorization: Bearer <jwt>` quand `auth_enabled`
    /// vaut true. Si le relais lui transmettait son propre jeton au meme
    /// endroit, le serveur chercherait un Bearer et n'en trouverait pas :
    /// acces refuse, sans que rien n'explique pourquoi.
    #[test]
    fn seul_le_jeton_de_pont_est_reconnu_comme_tel() {
        assert!(porte_un_jeton_de_pont("BridgeToken abc"));
        assert!(porte_un_jeton_de_pont("bridgetoken abc"));
        assert!(porte_un_jeton_de_pont("  BridgeToken abc"));
        assert!(!porte_un_jeton_de_pont("Basic dXNlcjpwYXNz"));
        assert!(!porte_un_jeton_de_pont(""));
        assert!(!porte_un_jeton_de_pont("BridgeTokenSansEspace"));
    }

    #[test]
    fn un_jeton_vide_vaut_absence() {
        assert_eq!(
            extraire_jeton(&entetes(&[(BRIDGE_TOKEN_HEADER, "  ")])),
            None
        );
        assert_eq!(
            extraire_jeton(&entetes(&[("authorization", "BridgeToken   ")])),
            None
        );
    }

    #[test]
    fn sans_rien_aucun_jeton() {
        assert_eq!(extraire_jeton(&HeaderMap::new()), None);
    }

    #[test]
    fn le_jeton_se_lit_dans_lurl_sous_ses_deux_noms() {
        assert_eq!(
            jeton_de_requete(Some("size=3&token=abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            jeton_de_requete(Some("bridge_token=a%2Bb")).as_deref(),
            Some("a+b")
        );
        assert_eq!(jeton_de_requete(Some("token=")), None);
        assert_eq!(jeton_de_requete(Some("tokens=abc")), None);
        assert_eq!(jeton_de_requete(None), None);
    }

    #[test]
    fn seul_le_parametre_qui_porte_le_jeton_est_retire() {
        assert_eq!(
            requete_sans_jeton(Some("size=3&token=abc&v=%20x"), "abc").as_deref(),
            Some("size=3&v=%20x")
        );
        assert_eq!(requete_sans_jeton(Some("bridge_token=abc"), "abc"), None);
        assert_eq!(
            requete_sans_jeton(Some("token=autre"), "abc").as_deref(),
            Some("token=autre")
        );
        assert_eq!(requete_sans_jeton(None, "abc"), None);
    }

    /// Le jeton de pont n'apparait dans AUCUNE trace, sous aucun de ses noms.
    #[test]
    fn le_jeton_est_masque_dans_les_traces() {
        let uri: Uri =
            "/api/relay/srv/library/artwork/a.jpg?size=3&token=SECRET&bridge_token=SECRET"
                .parse()
                .unwrap();
        let trace = uri_masquee(&uri);
        assert!(
            !trace.contains("SECRET"),
            "jeton en clair dans la trace : {trace}"
        );
        assert_eq!(
            trace,
            "/api/relay/srv/library/artwork/a.jpg?size=3&token=***&bridge_token=***"
        );
        let flux: Uri = "/stream/relay/srv/abc?token=SECRET".parse().unwrap();
        assert_eq!(uri_masquee(&flux), "/stream/relay/srv/abc?token=***");
    }
}
