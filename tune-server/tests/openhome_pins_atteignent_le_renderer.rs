//! Contre-preuve #2722 — « OpenHome Pins : le client attend un contrat que le
//! serveur ne fournit pas, et aucun appel n'atteint le service Pins du
//! renderer ».
//!
//! Ce que l'issue exige, mot pour mot : « un faux renderer Pins:1 publie
//! `DeviceMax`, une liste et observe Invoke/Set/Clear ; le GET Tune doit
//! restituer cette capacité et les commandes doivent atteindre le service ».
//!
//! Le piège que l'issue nomme est de faire vert en INVENTANT `max_slots` côté
//! Tune. Ce fichier le rend impossible de deux façons :
//!
//! * il appelle la ROUTE MONTÉE (`tune_server::routes::router`) et le vrai
//!   client de production (`tune_core::outputs::openhome_pins`), jamais une
//!   transcription ;
//! * il fait annoncer à DEUX faux appareils DEUX capacités différentes dans le
//!   même test. Aucun littéral Tune ne peut satisfaire les deux à la fois.
//!
//! Le chemin le plus fréquenté — un renderer qui ne porte PAS `Pins:1` — est
//! éprouvé lui aussi, y compris sur le délai : la fiche de zone ne doit pas
//! attendre le réseau pour dire « non pris en charge ».

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const CHEMIN_CONTROLE: &str = "/pins/control";

/// Capacités ANNONCÉES par les deux faux appareils. Deux valeurs distinctes,
/// et volontairement improbables comme littéral.
const DEVICE_MAX_A: u32 = 7;
const DEVICE_MAX_B: u32 = 3;

/// Identifiants publiés par `GetIdArray`.
const IDS_PUBLIES: &str = "11,12";

const TITRE_PUBLIE_A: &str = "Liste publiee par l appareil A";

/// Liste publiée par `ReadList`, dans la forme JSON du contrat OpenHome.
const LISTE_PUBLIEE: &str = r#"[
  {"id":11,"mode":"tidal","type":"playlist","uri":"tidal://playlist/aaa",
   "title":"Liste publiee par l appareil A","description":"premier","artworkUri":"http://x/a.jpg","shuffle":false},
  {"id":12,"mode":"qobuz","type":"album","uri":"qobuz://album/bbb",
   "title":"Second emplacement","description":"second","artworkUri":"","shuffle":true}
]"#;

// ── Le faux renderer ───────────────────────────────────────────────────────

/// Ce que le faux renderer a vu passer : (action, corps SOAP).
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<(String, String)>>>);

impl Journal {
    fn actions(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(action, _)| action.clone())
            .collect()
    }

    fn corps_de(&self, action: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(vue, _)| vue == action)
            .map(|(_, corps)| corps.clone())
    }
}

fn echapper_xml(texte: &str) -> String {
    texte
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn enveloppe(action: &str, charge: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:av-openhome-org:service:Pins:1">{charge}</u:{action}Response></s:Body></s:Envelope>"#
    )
}

/// Faux renderer `av.openhome.org:Pins:1` : il PUBLIE `DeviceMax` et une
/// liste, et OBSERVE toutes les actions reçues.
///
/// Écoute en IPv4 sur la boucle locale, sur un port éphémère lu après le
/// `bind` : plusieurs agents tournent en parallèle sur la même machine.
async fn faux_renderer_pins(device_max: u32, journal: Journal) -> u16 {
    let app = axum::Router::new().route(
        CHEMIN_CONTROLE,
        axum::routing::post(move |entetes: HeaderMap, corps: String| {
            let journal = journal.clone();
            async move {
                let action = entetes
                    .get("SOAPAction")
                    .and_then(|valeur| valeur.to_str().ok())
                    .unwrap_or("")
                    .trim_matches('"')
                    .rsplit('#')
                    .next()
                    .unwrap_or("")
                    .to_string();
                journal.0.lock().unwrap().push((action.clone(), corps));

                let charge = match action.as_str() {
                    "GetDeviceMax" => format!("<DeviceMax>{device_max}</DeviceMax>"),
                    "GetIdArray" => format!("<IdArray>{IDS_PUBLIES}</IdArray>"),
                    "ReadList" => format!("<List>{}</List>", echapper_xml(LISTE_PUBLIEE)),
                    "SetDevice" | "InvokeIndex" | "InvokeId" | "Clear" => String::new(),
                    _ => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            [(header::CONTENT_TYPE, "text/xml")],
                            "<faultstring>action inconnue de ce service</faultstring>".to_string(),
                        )
                            .into_response();
                    }
                };
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/xml")],
                    enveloppe(&action, &charge),
                )
                    .into_response()
            }
        }),
    );

    let ecouteur = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecouteur.local_addr().unwrap().port();
    // Le port est lié AVANT d'être rendu : aucune attente de démarrage n'est
    // nécessaire, la file d'attente TCP absorbe une connexion précoce.
    tokio::spawn(async move {
        axum::serve(ecouteur, app).await.ok();
    });
    port
}

/// Adresse qui accepte la connexion TCP et ne répond JAMAIS. Un appel SOAP y
/// coûterait les 5 secondes du plafond d'`openhome.rs` — c'est ce qui rend la
/// mesure de délai probante.
async fn trou_noir() -> (u16, tokio::net::TcpListener) {
    let ecouteur = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecouteur.local_addr().unwrap().port();
    (port, ecouteur)
}

// ── Montage serveur ────────────────────────────────────────────────────────

fn etat_et_routeur() -> (axum::Router, tune_server::state::AppState) {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let routeur = tune_server::routes::router(etat.clone());
    (routeur, etat)
}

/// Enregistre une VRAIE sortie OpenHome de production pointée sur le faux
/// renderer, et lui rattache une zone.
async fn zone_openhome(
    etat: &tune_server::state::AppState,
    device_id: &str,
    port: u16,
    services: &[(&str, &str)],
) -> i64 {
    let chemins: HashMap<String, String> = services
        .iter()
        .map(|(cle, chemin)| (cle.to_string(), chemin.to_string()))
        .collect();
    let sortie = tune_core::outputs::openhome::OpenHomeOutput::new(
        format!("Faux renderer {device_id}"),
        device_id.to_string(),
        "127.0.0.1".to_string(),
        port,
        chemins,
        None,
        HashMap::new(),
    );
    etat.outputs.lock().await.register(Box::new(sortie));
    tune_core::db::zone_repo::ZoneRepo::with_backend(etat.backend.clone())
        .create("Zone OpenHome", Some("openhome"), Some(device_id))
        .expect("creation de la zone temoin")
}

async fn envoyer(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let requete = match corps {
        Some(corps) => Request::builder()
            .method(methode)
            .uri(chemin)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
        None => Request::builder()
            .method(methode)
            .uri(chemin)
            .body(Body::empty())
            .unwrap(),
    };
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, json)
}

fn exiger_enveloppe_du_contrat_web(corps: &Value) {
    for champ in ["supported", "pins", "max_slots"] {
        assert!(
            corps.get(champ).is_some(),
            "docs/contrat-web.json exige `{champ}` sur GET /zones/{{}}/pins — recu: {corps}"
        );
    }
}

// ── Les épreuves ───────────────────────────────────────────────────────────

/// LE cœur de #2722 : `max_slots` est ce que l'APPAREIL annonce.
///
/// Deux faux renderers annoncent 7 et 3. Un `max_slots` littéral côté Tune ne
/// peut pas rendre les deux : ce test tombe dès qu'on en écrit un.
#[tokio::test]
async fn max_slots_et_liste_viennent_de_l_appareil_pas_d_un_litteral_tune() {
    let (app, etat) = etat_et_routeur();

    let journal_a = Journal::default();
    let port_a = faux_renderer_pins(DEVICE_MAX_A, journal_a.clone()).await;
    let zone_a = zone_openhome(&etat, "faux-pins-a", port_a, &[("pins", CHEMIN_CONTROLE)]).await;

    let journal_b = Journal::default();
    let port_b = faux_renderer_pins(DEVICE_MAX_B, journal_b.clone()).await;
    let zone_b = zone_openhome(&etat, "faux-pins-b", port_b, &[("pins", CHEMIN_CONTROLE)]).await;

    let (statut, corps) = envoyer(&app, "GET", &format!("/api/v1/zones/{zone_a}/pins"), None).await;
    assert_eq!(statut, StatusCode::OK, "GET pins appareil A: {corps}");
    exiger_enveloppe_du_contrat_web(&corps);

    assert_eq!(
        corps["supported"],
        json!(true),
        "l'appareil publie Pins:1, l'ecran doit le savoir: {corps}"
    );
    assert_eq!(
        corps["max_slots"],
        json!(DEVICE_MAX_A),
        "max_slots doit valoir le DeviceMax ANNONCE par l'appareil A: {corps}"
    );

    // La liste aussi vient de l'appareil, pas de `settings`.
    assert_eq!(
        corps["pins"].as_array().map(|liste| liste.len()),
        Some(2),
        "les deux pins publies par ReadList doivent etre rendus: {corps}"
    );
    assert_eq!(corps["pins"][0]["title"], json!(TITRE_PUBLIE_A), "{corps}");
    assert_eq!(corps["pins"][0]["id"], json!(11), "{corps}");
    assert_eq!(
        corps["pins"][1]["index"],
        json!(1),
        "le rang vient de la place dans l'IdArray de l'appareil: {corps}"
    );
    assert_eq!(corps["pins"][1]["shuffle"], json!(true), "{corps}");

    // Les trois actions de LECTURE ont bien atteint le service du renderer.
    let actions = journal_a.actions();
    for attendue in ["GetDeviceMax", "GetIdArray", "ReadList"] {
        assert!(
            actions.iter().any(|vue| vue == attendue),
            "l'action {attendue} n'a jamais atteint le service Pins:1 — vues: {actions:?}"
        );
    }

    // Le second appareil annonce une AUTRE capacite. Un litteral tomberait ici.
    let (statut, corps_b) =
        envoyer(&app, "GET", &format!("/api/v1/zones/{zone_b}/pins"), None).await;
    assert_eq!(statut, StatusCode::OK, "GET pins appareil B: {corps_b}");
    assert_eq!(
        corps_b["max_slots"],
        json!(DEVICE_MAX_B),
        "deux appareils annoncent deux capacites differentes ({DEVICE_MAX_A} et {DEVICE_MAX_B}) : \
         aucun litteral Tune ne peut satisfaire les deux — {corps_b}"
    );
    assert!(
        journal_b.actions().iter().any(|vue| vue == "GetDeviceMax"),
        "l'appareil B n'a jamais ete interroge"
    );
}

/// Set / Invoke / Clear doivent ATTEINDRE le service, pas `settings`.
#[tokio::test]
async fn set_invoke_et_clear_atteignent_le_service_pins_du_renderer() {
    let (app, etat) = etat_et_routeur();
    let journal = Journal::default();
    let port = faux_renderer_pins(DEVICE_MAX_A, journal.clone()).await;
    let zone = zone_openhome(&etat, "faux-pins-cmd", port, &[("pins", CHEMIN_CONTROLE)]).await;

    // ── SetDevice
    let (statut, corps) = envoyer(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone}/pins"),
        Some(json!({
            "index": 2,
            "title": "Pose par le test",
            "uri": "tidal://playlist/ecrit-par-le-test",
            "type": "playlist",
            "mode": "tidal",
            "shuffle": true
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "POST pins: {corps}");
    let soap = journal
        .corps_de("SetDevice")
        .expect("SetDevice n'a jamais atteint le service Pins:1");
    assert!(soap.contains("<Index>2</Index>"), "SetDevice: {soap}");
    assert!(
        soap.contains("<Uri>tidal://playlist/ecrit-par-le-test</Uri>"),
        "SetDevice: {soap}"
    );
    assert!(soap.contains("<Mode>tidal</Mode>"), "SetDevice: {soap}");
    assert!(soap.contains("<Shuffle>1</Shuffle>"), "SetDevice: {soap}");

    // ── InvokeIndex
    let (statut, corps) = envoyer(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone}/pins/0/invoke"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::ACCEPTED, "POST invoke: {corps}");
    let soap = journal
        .corps_de("InvokeIndex")
        .expect("InvokeIndex n'a jamais atteint le service Pins:1");
    assert!(soap.contains("<Index>0</Index>"), "InvokeIndex: {soap}");

    // ── Clear : le rang 1 de l'URL doit se traduire en l'identifiant 12 que
    //    l'appareil a publie dans son IdArray.
    let (statut, corps) = envoyer(
        &app,
        "DELETE",
        &format!("/api/v1/zones/{zone}/pins/1"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT, "DELETE pin: {corps}");
    let soap = journal
        .corps_de("Clear")
        .expect("Clear n'a jamais atteint le service Pins:1");
    assert!(
        soap.contains("<Id>12</Id>"),
        "Clear doit porter l'identifiant publie par l'appareil, pas le rang: {soap}"
    );
}

/// Le chemin le plus fréquenté : un renderer SANS service `Pins:1`.
///
/// `supported` doit valoir `false` honnêtement, et l'écran ne doit attendre
/// AUCUN délai réseau. Le renderer est ici un trou noir TCP : le moindre appel
/// SOAP coûterait les 5 secondes du plafond.
#[tokio::test]
async fn renderer_sans_service_pins_repond_non_supporte_sans_attendre_le_reseau() {
    let (app, etat) = etat_et_routeur();
    let (port, _ecouteur_maintenu_en_vie) = trou_noir().await;
    let zone = zone_openhome(
        &etat,
        "openhome-sans-pins",
        port,
        &[
            ("playlist", "/playlist/control"),
            ("product", "/product/control"),
        ],
    )
    .await;

    let debut = std::time::Instant::now();
    let (statut, corps) = envoyer(&app, "GET", &format!("/api/v1/zones/{zone}/pins"), None).await;
    let ecoule = debut.elapsed();

    assert_eq!(statut, StatusCode::OK, "{corps}");
    exiger_enveloppe_du_contrat_web(&corps);
    assert_eq!(
        corps["supported"],
        json!(false),
        "un renderer sans Pins:1 doit le dire honnetement: {corps}"
    );
    assert_eq!(
        corps["max_slots"],
        json!(0),
        "aucune capacite inventee: {corps}"
    );
    assert_eq!(corps["pins"], json!([]), "{corps}");
    assert!(
        ecoule < std::time::Duration::from_millis(500),
        "la fiche de zone a attendu {ecoule:?} : elle a interroge le reseau alors que \
         le descriptif de l'appareil suffisait a repondre"
    );
}

/// Une zone qui n'est branchée à aucune sortie OpenHome — le cas de la
/// majorité des zones (navigateur, local, DLNA).
#[tokio::test]
async fn zone_sans_sortie_openhome_repond_non_supporte() {
    let (app, etat) = etat_et_routeur();
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(etat.backend.clone())
        .create("Zone navigateur", Some("browser"), Some("navigateur-2722"))
        .expect("creation de la zone temoin");

    let debut = std::time::Instant::now();
    let (statut, corps) = envoyer(&app, "GET", &format!("/api/v1/zones/{zone}/pins"), None).await;
    let ecoule = debut.elapsed();

    assert_eq!(statut, StatusCode::OK, "{corps}");
    exiger_enveloppe_du_contrat_web(&corps);
    assert_eq!(corps["supported"], json!(false), "{corps}");
    assert_eq!(corps["max_slots"], json!(0), "{corps}");
    assert!(
        ecoule < std::time::Duration::from_millis(500),
        "la fiche de zone a attendu {ecoule:?} sans meme avoir de renderer a interroger"
    );
}

/// Un appareil qui publie `Pins:1` mais ne répond pas ne doit pas faire
/// inventer une capacité : `supported` retombe à `false` et l'erreur est dite.
#[tokio::test]
async fn renderer_pins_injoignable_n_invente_aucune_capacite() {
    let (app, etat) = etat_et_routeur();
    let journal = Journal::default();
    let port = faux_renderer_pins(DEVICE_MAX_A, journal.clone()).await;
    // Un chemin de contrôle que le faux renderer ne sert pas : la requête part
    // vraiment, et revient en 404.
    let zone = zone_openhome(&etat, "pins-muet", port, &[("pins", "/pins/absent")]).await;

    let (statut, corps) = envoyer(&app, "GET", &format!("/api/v1/zones/{zone}/pins"), None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    exiger_enveloppe_du_contrat_web(&corps);
    assert_eq!(corps["supported"], json!(false), "{corps}");
    assert_eq!(
        corps["max_slots"],
        json!(0),
        "une lecture en echec ne doit produire AUCUNE capacite: {corps}"
    );
    assert!(
        corps.get("error").is_some(),
        "l'echec doit se dire au lieu de passer pour « pas de pins »: {corps}"
    );
}
