//! Le relais d'API vu de l'exterieur : un vrai pont sur 127.0.0.1, un vrai
//! client HTTP, et un faux serveur Tune au bout du canal WebSocket.
//!
//! Pourquoi en boite noire : les quatre manques constates le 09/10/2026 par le
//! pont (pochettes `<img>`, liens d'export `<a href>`, profil, cache) sont des
//! comportements de bout en bout. Les verifier par l'API publique du pont —
//! une requete HTTP entre, une `relay.request` sort — garde le test vrai quoi
//! qu'il arrive a l'implementation.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use tokio::sync::mpsc;

use crate::state::RelayState;

const JETON: &str = "jeton-du-pont-0123456789";
const PATIENCE: Duration = Duration::from_secs(5);

struct PontDeTest {
    base: String,
    state: Arc<RelayState>,
    serveur: mpsc::Receiver<String>,
}

async fn pont_de_test() -> PontDeTest {
    let state = Arc::new(RelayState::new());
    let (tx, serveur) = mpsc::channel::<String>(16);
    state
        .register_server("srv".into(), "Salon".into(), JETON.into(), tx)
        .unwrap();
    let app = Router::new()
        .route(
            "/api/relay/{server_id}/{*path}",
            any(crate::api_proxy::proxy_api),
        )
        .with_state(state.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    PontDeTest {
        base: format!("http://{adresse}/api/relay/srv"),
        state,
        serveur,
    }
}

/// Envoie la requete ; si le pont la relaie, le faux serveur repond par
/// `reponse(requete_relayee)`. Rend la requete relayee (ou `None` si le pont
/// a repondu seul, un 401 par exemple) et la reponse HTTP.
async fn echange(
    pont: &mut PontDeTest,
    requete: reqwest::RequestBuilder,
    reponse: impl FnOnce(&serde_json::Value) -> serde_json::Value,
) -> (Option<serde_json::Value>, u16, Vec<u8>) {
    let envoi = tokio::spawn(async move { requete.send().await.unwrap() });
    let relayee = tokio::select! {
        trame = pont.serveur.recv() => trame,
        _ = tokio::time::sleep(Duration::from_millis(500)) => None,
    };
    let relayee = relayee.map(|t| serde_json::from_str::<serde_json::Value>(&t).unwrap());
    if let Some(r) = &relayee {
        let mut corps = reponse(r);
        corps["type"] = "relay.response".into();
        corps["id"] = r["id"].clone();
        crate::ws_server::handle_server_message(&pont.state, "srv", &corps.to_string()).await;
    }
    let rep = tokio::time::timeout(PATIENCE, envoi)
        .await
        .expect("le pont n'a jamais repondu")
        .unwrap();
    let statut = rep.status().as_u16();
    let octets = rep.bytes().await.unwrap().to_vec();
    (relayee, statut, octets)
}

fn ok_json(_: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"status": 200, "headers": {"content-type": "application/json"}, "body": "{}"})
}

/// Une balise `<img>` ne pose aucun en-tete : le jeton vient dans l'URL.
#[tokio::test]
async fn un_get_porte_son_jeton_dans_lurl() {
    let mut pont = pont_de_test().await;
    let url = format!("{}/library/artwork/abc.jpg?token={JETON}", pont.base);
    let (relayee, statut, _) = echange(&mut pont, reqwest::Client::new().get(url), ok_json).await;
    assert_eq!(statut, 200, "le jeton en parametre doit ouvrir le relais");
    assert!(relayee.is_some());
}

/// Meme chose sous le nom long, que rien d'autre ne peut revendiquer.
#[tokio::test]
async fn le_parametre_bridge_token_est_accepte_aussi() {
    let mut pont = pont_de_test().await;
    let url = format!("{}/history/export?bridge_token={JETON}", pont.base);
    let (_, statut, _) = echange(&mut pont, reqwest::Client::new().get(url), ok_json).await;
    assert_eq!(statut, 200);
}

/// LE point de securite : le jeton du pont ne doit JAMAIS atteindre le
/// serveur (journaux d'acces, historique de requetes). Les autres parametres,
/// eux, doivent arriver intacts — `?size=` choisit la vignette.
#[tokio::test]
async fn le_jeton_est_retire_et_le_reste_de_la_requete_relaye() {
    let mut pont = pont_de_test().await;
    let url = format!(
        "{}/library/artwork/abc.jpg?size=300&token={JETON}&v=2",
        pont.base
    );
    let (relayee, statut, _) = echange(&mut pont, reqwest::Client::new().get(url), ok_json).await;
    assert_eq!(statut, 200);
    let relayee = relayee.expect("requete non relayee");
    let chemin = relayee["path"].as_str().unwrap();
    assert!(
        !chemin.contains(JETON),
        "jeton relaye au serveur : {chemin}"
    );
    assert!(
        !relayee.to_string().contains(JETON),
        "jeton present dans la trame"
    );
    assert_eq!(chemin, "/api/v1/library/artwork/abc.jpg?size=300&v=2");
}

/// Un parametre `token` qui n'est PAS le jeton du pont appartient au serveur :
/// le pont n'y touche pas.
#[tokio::test]
async fn un_autre_parametre_token_passe_intact() {
    let mut pont = pont_de_test().await;
    let url = format!("{}/x?token=autre-chose", pont.base);
    let requete = reqwest::Client::new()
        .get(url)
        .header("x-bridge-token", JETON);
    let (relayee, statut, _) = echange(&mut pont, requete, ok_json).await;
    assert_eq!(statut, 200);
    assert_eq!(relayee.unwrap()["path"], "/api/v1/x?token=autre-chose");
}

/// Le jeton en URL ne vaut que pour la LECTURE : un POST garde l'en-tete.
/// Un formulaire tiers ne doit pas pouvoir ecrire a travers le pont avec un
/// lien piege.
#[tokio::test]
async fn un_post_nest_pas_ouvert_par_le_jeton_dans_lurl() {
    let mut pont = pont_de_test().await;
    let url = format!("{}/zones/1/play?token={JETON}", pont.base);
    let (relayee, statut, _) = echange(&mut pont, reqwest::Client::new().post(url), ok_json).await;
    assert_eq!(statut, 401);
    assert!(relayee.is_none());
}

#[tokio::test]
async fn un_mauvais_jeton_dans_lurl_est_refuse() {
    let mut pont = pont_de_test().await;
    let url = format!("{}/library/artwork/abc.jpg?token=faux", pont.base);
    let (relayee, statut, _) = echange(&mut pont, reqwest::Client::new().get(url), ok_json).await;
    assert_eq!(statut, 401);
    assert!(relayee.is_none());
}

/// Sans `X-Tune-Profile`, le serveur rend les favoris et l'historique du
/// profil par defaut : chacun voit ceux d'un autre.
#[tokio::test]
async fn len_tete_de_profil_est_relaye() {
    let mut pont = pont_de_test().await;
    let requete = reqwest::Client::new()
        .get(format!("{}/favorites", pont.base))
        .header("x-bridge-token", JETON)
        .header("X-Tune-Profile", "3");
    let (relayee, _, _) = echange(&mut pont, requete, ok_json).await;
    assert_eq!(relayee.unwrap()["headers"]["x-tune-profile"], "3");
}

/// Range et validateurs de cache restent relayes ; le jeton du pont, lui,
/// ne part jamais en en-tete vers le serveur.
#[tokio::test]
async fn range_et_cache_restent_relayes_et_le_jeton_non() {
    let mut pont = pont_de_test().await;
    let requete = reqwest::Client::new()
        .get(format!("{}/library/artwork/abc.jpg", pont.base))
        .header("x-bridge-token", JETON)
        .header("range", "bytes=0-99")
        .header("if-none-match", "\"abc\"")
        .header("if-modified-since", "Thu, 08 Oct 2026 10:00:00 GMT")
        .header("cache-control", "no-cache")
        .header("cookie", "tune_session=secret");
    let (relayee, _, _) = echange(&mut pont, requete, ok_json).await;
    let h = &relayee.unwrap()["headers"];
    assert_eq!(h["range"], "bytes=0-99");
    assert_eq!(h["if-none-match"], "\"abc\"");
    assert_eq!(h["if-modified-since"], "Thu, 08 Oct 2026 10:00:00 GMT");
    assert_eq!(h["cache-control"], "no-cache");
    assert!(h.get("x-bridge-token").is_none());
    assert!(h.get("cookie").is_none(), "liste blanche : pas de cookie");
}

/// Une pochette est binaire. Le serveur l'envoie en `body_base64` ; le pont
/// doit rendre les octets exacts, sinon l'image est cassee.
#[tokio::test]
async fn un_corps_binaire_traverse_intact() {
    let mut pont = pont_de_test().await;
    let url = format!("{}/library/artwork/abc.jpg?token={JETON}", pont.base);
    // FF D8 FF E0 : en-tete JPEG, invalide en UTF-8.
    let (_, statut, octets) = echange(&mut pont, reqwest::Client::new().get(url), |_| {
        serde_json::json!({
            "status": 200,
            "headers": {"content-type": "image/jpeg"},
            "body_base64": "/9j/4A==",
        })
    })
    .await;
    assert_eq!(statut, 200);
    assert_eq!(octets, vec![0xFF, 0xD8, 0xFF, 0xE0]);
}

/// Un serveur plus ancien envoie toujours `body` en texte : rien ne change.
#[tokio::test]
async fn un_corps_texte_dun_ancien_serveur_reste_lu() {
    let mut pont = pont_de_test().await;
    let requete = reqwest::Client::new()
        .get(format!("{}/zones", pont.base))
        .header("x-bridge-token", JETON);
    let (_, statut, octets) = echange(
        &mut pont,
        requete,
        |_| serde_json::json!({"status": 200, "headers": {}, "body": "[1,2]"}),
    )
    .await;
    assert_eq!(statut, 200);
    assert_eq!(octets, b"[1,2]".to_vec());
}
