//! #4895 — « serveurs multimédia s'ouvre mais reste bloqué » (Belkadi Yacine,
//! fil 1909, ticket 160, 0.9.163) : la navigation dans la Freebox s'arrête au
//! dossier `Freebox`, **sans bandeau d'erreur et sans aucune ligne
//! `browse_media_server_failed` au journal**.
//!
//! # Ce que le code de la 0.9.163 faisait
//!
//! `parcourir_les_enfants_verifie` (routes/network.rs) sait qu'un `Browse` a
//! mal tourné : il le note dans `erreur`. Mais la route ne transforme en
//! erreur HTTP que `cause`, et `cause` n'était posée qu'aux échecs de
//! transport, de statut et de SOAP Fault (#4134). Trois échecs de la PREMIÈRE
//! page restaient donc sans cause, et la route rendait **200, listes vides** —
//! un dossier vide, pas un échec :
//!
//! 1. `NumberReturned=0` alors que `TotalMatches=12` : le serveur annonce douze
//!    enfants et n'en rend aucun (« pagination interrompue (0/12) », le relevé
//!    de l'import UPnP du .18 en compte 34 de cette forme) ;
//! 2. un DIDL que Tune ne sait pas lire en entier (`parsed < NumberReturned`) ;
//! 3. une réponse sans compteur `NumberReturned`.
//!
//! Et une page vide AU MILIEU de la pagination (« 8/13 ») laissait passer ce
//! qui avait été lu, en 200, sans que la réponse dise qu'il manquait la suite.
//!
//! # Le banc
//!
//! Un vrai serveur HTTP répond du SOAP `Browse` réel, page par page selon
//! `StartingIndex` ; la route est appelée par le routeur réel. La preuve ne
//! passe pas par une Freebox (aucune sur Shrek) : elle passe par ce rejeu des
//! quatre formes de réponse que le code ne savait pas dire.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` de `Cargo.toml`, sans quoi il ne serait jamais compilé.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

const ID: &str = "banc-4895";

fn echapper(texte: &str) -> String {
    texte
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Une réponse `Browse` réelle. `rendus = None` retire `NumberReturned`.
fn enveloppe(didl: &str, rendus: Option<u32>, total: u32) -> String {
    let rendus = rendus
        .map(|n| format!("<NumberReturned>{n}</NumberReturned>"))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\"?><s:Envelope \
         xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
         <Result>{}</Result>{rendus}<TotalMatches>{total}</TotalMatches>\
         <UpdateID>7</UpdateID></u:BrowseResponse></s:Body></s:Envelope>",
        echapper(&format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{didl}</DIDL-Lite>"
        ))
    )
}

fn dossier(id: &str, titre: &str) -> String {
    format!(
        "<container id=\"{id}\" parentID=\"p\" restricted=\"1\" childCount=\"3\">\
         <dc:title>{titre}</dc:title>\
         <upnp:class>object.container.storageFolder</upnp:class></container>"
    )
}

fn dossiers(prefixe: &str, n: u32) -> String {
    (0..n)
        .map(|i| dossier(&format!("{prefixe}-{i}"), &format!("Album {i}")))
        .collect()
}

/// Le ContentDirectory du banc. Chaque `ObjectID` rejoue une forme de réponse.
async fn lever_le_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse liée");
    let app = Router::new().route(
        "/control",
        post(|corps: String| async move {
            let champ = |nom: &str| {
                corps
                    .split_once(&format!("<{nom}>"))
                    .and_then(|(_, r)| r.split_once(&format!("</{nom}>")))
                    .map(|(v, _)| v.trim().to_string())
                    .unwrap_or_default()
            };
            let depart: u32 = champ("StartingIndex").parse().unwrap_or(0);
            match champ("ObjectID").as_str() {
                // Un dossier qui annonce 12 enfants et n'en rend aucun.
                "musiques-vide" => enveloppe("", Some(0), 12),
                // Trois éléments annoncés, rendus sous une forme que Tune ne
                // lit pas (des `<item>` sans attribut) : XML complet, DIDL
                // illisible pour l'analyseur.
                "didl-illisible" => enveloppe(
                    "<item><dc:title>a</dc:title></item>\
                     <item><dc:title>b</dc:title></item>\
                     <item><dc:title>c</dc:title></item>",
                    Some(3),
                    3,
                ),
                // Pas de `NumberReturned` du tout.
                "sans-compteur" => enveloppe(&dossiers("sc", 2), None, 2),
                // 13 annoncés : une première page de 8, puis une page vide.
                "milieu-vide" if depart == 0 => enveloppe(&dossiers("mv", 8), Some(8), 13),
                "milieu-vide" => enveloppe("", Some(0), 13),
                // Le cas sain : deux pages, 3 + 2, rien ne manque.
                "complet" if depart == 0 => enveloppe(&dossiers("ok", 3), Some(3), 5),
                "complet" if depart == 3 => enveloppe(&dossiers("ok2", 2), Some(2), 5),
                // Un dossier réellement vide.
                _ => enveloppe("", Some(0), 0),
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}/control")
}

async fn banc() -> Router {
    let controle = lever_le_banc().await;
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    etat.media_servers.lock().await.insert(
        ID.to_string(),
        tune_core::discovery::ssdp::MediaServerInfo {
            id: ID.to_string(),
            name: "Freebox du banc".to_string(),
            manufacturer: "Freebox SA".to_string(),
            model: "Freebox Server".to_string(),
            location: controle.clone(),
            content_directory_url: controle,
            host: "127.0.0.1".to_string(),
            port: 0,
            last_seen: std::time::Instant::now(),
            max_age: std::time::Duration::from_secs(1800),
        },
    );
    tune_server::routes::router(etat)
}

async fn parcourir(app: &Router, object_id: &str) -> (StatusCode, Value) {
    let uri = format!("/api/v1/network/media-servers/{ID}/browse?object_id={object_id}");
    let reponse = app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .expect("routeur en échec");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    let corps = serde_json::from_slice(&octets).unwrap_or_else(|e| {
        panic!(
            "{uri} : JSON illisible ({e}) — {}",
            String::from_utf8_lossy(&octets)
        )
    });
    (statut, corps)
}

/// L'échec doit être DIT : pas un 200, le code stable de #4134, et un message
/// qui nomme le serveur. (La même porte `en_erreur_http` écrit la ligne
/// `browse_media_server_failed` au journal.)
fn exige_un_echec_dit(statut: StatusCode, corps: &Value, forme: &str, attendu: &str) {
    assert_eq!(
        statut,
        StatusCode::BAD_GATEWAY,
        "{forme} : un Browse qui ne peut pas être servi a rendu {statut} — le \
         dossier vide muet de #4895.\n{corps}"
    );
    assert_eq!(
        corps["code"], "media_server_browse_failed",
        "{forme} : code stable absent.\n{corps}"
    );
    let message = corps["error"]
        .as_str()
        .or(corps["message"].as_str())
        .unwrap_or("");
    assert!(
        message.contains("Freebox du banc") && message.contains(attendu),
        "{forme} : le message doit nommer le serveur et dire « {attendu} ».\n{corps}"
    );
}

#[tokio::test]
async fn une_premiere_page_vide_sur_douze_annonces_est_un_echec_dit() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "musiques-vide").await;
    exige_un_echec_dit(statut, &corps, "0 rendu sur 12 annoncés", "0 des 12");
}

#[tokio::test]
async fn un_didl_illisible_est_un_echec_dit() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "didl-illisible").await;
    exige_un_echec_dit(statut, &corps, "DIDL illisible", "0 des 3");
}

#[tokio::test]
async fn une_reponse_sans_compteur_est_un_echec_dit() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "sans-compteur").await;
    exige_un_echec_dit(statut, &corps, "sans NumberReturned", "NumberReturned");
}

#[tokio::test]
async fn une_page_vide_au_milieu_n_est_pas_une_fin_de_liste() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "milieu-vide").await;
    // Ce qui a été lu reste servi : l'écran peut montrer 8 albums sur 13…
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["containers"].as_array().map(Vec::len),
        Some(8),
        "{corps}"
    );
    assert_eq!(corps["total_matches"], 13, "{corps}");
    // … mais la réponse DIT qu'il en manque, et pourquoi.
    assert_eq!(
        corps["complet"], false,
        "8 lus sur 13 annoncés, puis une page vide : la réponse doit porter \
         `complet: false`, pas se lire comme une fin de liste.\n{corps}"
    );
    let raison = corps["incomplet"].as_str().unwrap_or("");
    assert!(
        raison.contains("8/13"),
        "`incomplet` doit dire où le parcours s'est arrêté.\n{corps}"
    );
}

/// Contrôle : le cas sain reste un succès COMPLET. Sans lui, un correctif qui
/// marquerait tout « incomplet » passerait les témoins ci-dessus.
#[tokio::test]
async fn un_dossier_sain_ou_vide_reste_un_succes_complet() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "complet").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["containers"].as_array().map(Vec::len),
        Some(5),
        "{corps}"
    );
    assert_eq!(corps["complet"], true, "{corps}");
    assert!(corps["incomplet"].is_null(), "{corps}");

    let (statut, corps) = parcourir(&app, "vide-reel").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["containers"].as_array().map(Vec::len),
        Some(0),
        "{corps}"
    );
    assert_eq!(
        corps["complet"], true,
        "un dossier vide n'est pas un échec.\n{corps}"
    );
}
