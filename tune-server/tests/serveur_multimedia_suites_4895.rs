//! Suites de #4895 (relevées le 24/09/2026 en corrigeant le Browse, PR #4914).
//!
//! # 1. La route `search` s'arrêtait EN SILENCE
//!
//! `GET /network/media-servers/{id}/search` a sa propre boucle de pagination.
//! Elle sortait sur toute page vide, sans rien dire : une page vide AU MILIEU
//! (« 8 rendus sur 13 annoncés, puis 0 ») servait 8 résultats comme s'ils
//! étaient tous, et une première page vide sur 12 annoncés se lisait « aucun
//! résultat ». Même règle que le Browse de #4914 : 502 typé quand rien n'a
//! été lu, `complet: false` + `incomplet` au-delà.
//!
//! # 2. L'analyseur DIDL exigeait une ESPACE après le nom de balise
//!
//! `<item ` et `<container ` étaient cherchés avec l'espace : `<item\n id=…>`
//! ou `<container\tid=…>` — du XML valide — rendaient le DIDL illisible
//! (502 `DidlIllisible` depuis #4914, dossier vide avant). Le témoin exige
//! aussi que `<itemfoo>` ne soit PAS lu comme un `<item>`.
//!
//! Le banc : un vrai serveur HTTP rejoue du SOAP `Browse`, `Search` et
//! `GetSearchCapabilities` ; les routes sont appelées par le routeur réel.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` de `Cargo.toml`, sans quoi il ne serait jamais compilé.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

const ID: &str = "banc-suites-4895";

fn echapper(texte: &str) -> String {
    texte
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Une réponse ContentDirectory réelle (`BrowseResponse` / `SearchResponse`).
fn enveloppe(action: &str, didl: &str, rendus: u32, total: u32) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope \
         xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
         <Result>{}</Result><NumberReturned>{rendus}</NumberReturned>\
         <TotalMatches>{total}</TotalMatches>\
         <UpdateID>7</UpdateID></u:{action}Response></s:Body></s:Envelope>",
        echapper(&format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{didl}</DIDL-Lite>"
        ))
    )
}

fn piste(id: &str) -> String {
    format!(
        "<item id=\"{id}\" parentID=\"p\" restricted=\"1\">\
         <dc:title>Titre {id}</dc:title>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/flac:*\">http://127.0.0.1/{id}.flac</res>\
         </item>"
    )
}

fn pistes(prefixe: &str, n: u32) -> String {
    (0..n).map(|i| piste(&format!("{prefixe}-{i}"))).collect()
}

/// Le ContentDirectory du banc. Le conteneur visé choisit la forme rejouée.
async fn lever_le_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse liée");
    let app = Router::new().route(
        "/control",
        post(|entetes: HeaderMap, corps: String| async move {
            let action = entetes
                .get("SOAPAction")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let champ = |nom: &str| {
                corps
                    .split_once(&format!("<{nom}>"))
                    .and_then(|(_, r)| r.split_once(&format!("</{nom}>")))
                    .map(|(v, _)| v.trim().to_string())
                    .unwrap_or_default()
            };
            let depart: u32 = champ("StartingIndex").parse().unwrap_or(0);
            if action.contains("#GetSearchCapabilities") {
                return "<?xml version=\"1.0\"?><s:Envelope \
                        xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                        <u:GetSearchCapabilitiesResponse \
                        xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
                        <SearchCaps>dc:title,upnp:class</SearchCaps>\
                        </u:GetSearchCapabilitiesResponse></s:Body></s:Envelope>"
                    .to_string();
            }
            if action.contains("#Search") {
                return match champ("ContainerID").as_str() {
                    // 12 annoncés, aucun rendu.
                    "premiere-vide" => enveloppe("Search", "", 0, 12),
                    // 13 annoncés : une page de 8, puis une page vide.
                    "milieu-vide" if depart == 0 => enveloppe("Search", &pistes("mv", 8), 8, 13),
                    "milieu-vide" => enveloppe("Search", "", 0, 13),
                    // Le cas sain : 3 + 2 sur 5.
                    _ if depart == 0 => enveloppe("Search", &pistes("ok", 3), 3, 5),
                    _ if depart == 3 => enveloppe("Search", &pistes("ok2", 2), 2, 5),
                    _ => enveloppe("Search", "", 0, 5),
                };
            }
            // Browse
            match champ("ObjectID").as_str() {
                // Saut de ligne, tabulation, CR-LF après le nom de balise.
                "blancs" => enveloppe(
                    "Browse",
                    "<container\n id=\"c1\" parentID=\"p\" restricted=\"1\" childCount=\"2\">\
                     <dc:title>Dossier LF</dc:title>\
                     <upnp:class>object.container.storageFolder</upnp:class></container>\
                     <container\tid=\"c2\" parentID=\"p\" restricted=\"1\">\
                     <dc:title>Dossier TAB</dc:title>\
                     <upnp:class>object.container.storageFolder</upnp:class></container>\
                     <item\r\n id=\"i1\" parentID=\"p\" restricted=\"1\">\
                     <dc:title\n xml:lang=\"fr\">Piste CRLF</dc:title>\
                     <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
                     <res\n protocolInfo=\"http-get:*:audio/flac:*\">http://127.0.0.1/i1.flac</res>\
                     </item>",
                    3,
                    3,
                ),
                // Une balise `<itemfoo>` suivie d'un vrai `<item>` : UN élément.
                "itemfoo" => enveloppe(
                    "Browse",
                    "<itemfoo id=\"faux\" parentID=\"p\"><dc:title>Faux</dc:title></itemfoo>\
                     <item\n id=\"vrai\" parentID=\"p\" restricted=\"1\">\
                     <dc:title>Vrai</dc:title>\
                     <upnp:class>object.item.audioItem.musicTrack</upnp:class></item>",
                    1,
                    1,
                ),
                _ => enveloppe("Browse", "", 0, 0),
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
            name: "NAS du banc".to_string(),
            manufacturer: "Banc".to_string(),
            model: "ContentDirectory".to_string(),
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

async fn appeler(app: &Router, uri: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
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

async fn chercher(app: &Router, conteneur: &str) -> (StatusCode, Value) {
    appeler(
        app,
        &format!("/api/v1/network/media-servers/{ID}/search?q=titre&container={conteneur}"),
    )
    .await
}

async fn parcourir(app: &Router, object_id: &str) -> (StatusCode, Value) {
    appeler(
        app,
        &format!("/api/v1/network/media-servers/{ID}/browse?object_id={object_id}"),
    )
    .await
}

// ── 1. search ────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_une_premiere_page_vide_sur_douze_annonces_est_un_echec_dit() {
    let app = banc().await;
    let (statut, corps) = chercher(&app, "premiere-vide").await;
    assert_eq!(
        statut,
        StatusCode::BAD_GATEWAY,
        "Search : 0 rendu sur 12 annoncés a rendu {statut} — « aucun résultat » \
         muet au lieu d'un échec dit.\n{corps}"
    );
    assert_eq!(corps["code"], "media_server_search_failed", "{corps}");
    let message = corps["error"]
        .as_str()
        .or(corps["message"].as_str())
        .unwrap_or("");
    assert!(
        message.contains("NAS du banc") && message.contains("0 des 12"),
        "le message doit nommer le serveur et dire « 0 des 12 ».\n{corps}"
    );
}

#[tokio::test]
async fn search_une_page_vide_au_milieu_n_est_pas_une_fin_de_liste() {
    let app = banc().await;
    let (statut, corps) = chercher(&app, "milieu-vide").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["items"].as_array().map(Vec::len), Some(8), "{corps}");
    assert_eq!(corps["total_matches"], 13, "{corps}");
    assert_eq!(
        corps["complet"], false,
        "Search : 8 lus sur 13 annoncés, puis une page vide — la réponse doit \
         porter `complet: false`, pas se lire comme la liste entière.\n{corps}"
    );
    let raison = corps["incomplet"].as_str().unwrap_or("");
    assert!(
        raison.contains("8 des 13"),
        "`incomplet` doit dire où la recherche s'est arrêtée.\n{corps}"
    );
}

/// Contrôle : une recherche saine sur deux pages reste un succès COMPLET.
#[tokio::test]
async fn search_saine_sur_deux_pages_reste_complete() {
    let app = banc().await;
    let (statut, corps) = chercher(&app, "sain").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["items"].as_array().map(Vec::len), Some(5), "{corps}");
    assert_eq!(corps["complet"], true, "{corps}");
    assert!(corps["incomplet"].is_null(), "{corps}");
}

// ── 2. DIDL ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn didl_tout_blanc_xml_apres_le_nom_de_balise_est_lu() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "blancs").await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "`<container\\n id=…>`, `<container\\tid=…>`, `<item\\r\\n id=…>` : du XML \
         valide que l'analyseur ne sait pas lire.\n{corps}"
    );
    let titres: Vec<&str> = corps["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["title"].as_str())
        .collect();
    assert_eq!(titres, ["Dossier LF", "Dossier TAB"], "{corps}");
    let item = &corps["items"][0];
    assert_eq!(item["id"], "i1", "{corps}");
    assert_eq!(item["title"], "Piste CRLF", "{corps}");
    assert_eq!(
        item["res_url"], "http://127.0.0.1/i1.flac",
        "`<res\\n protocolInfo=…>` doit être lu.\n{corps}"
    );
}

#[tokio::test]
async fn didl_itemfoo_n_est_pas_un_item() {
    let app = banc().await;
    let (statut, corps) = parcourir(&app, "itemfoo").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let ids: Vec<&str> = corps["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        ["vrai"],
        "`<itemfoo>` a été lu comme un `<item>`.\n{corps}"
    );
}
