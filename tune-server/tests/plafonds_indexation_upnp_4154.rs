//! Les trois plafonds de l'indexation UPnP se règlent, se souviennent, et
//! **disent quand ils mordent** (#4154).
//!
//! # Le défaut
//!
//! `PISTES_DEFAUT = 50_000` avait été calé en phase 2 sur les 22 331 pistes
//! d'Asset. La bibliothèque locale de Bertrand en compte **47 056** (mesuré sur
//! le `.18` le 14/09/2026) : **6 % de marge**. Un Tune qui en indexerait un
//! autre aurait été tronqué presque aussitôt.
//!
//! Et **en silence** : `bilan.plafond_atteint = Some("pistes")` puis `break`
//! finissait dans une phrase qui ne nommait aucun réglage —
//! « relancer sur un conteneur plus précis ». L'utilisateur cherche un album
//! jamais indexé, ne le trouve pas, et croit à un bug de recherche.
//!
//! # Ce que cette épreuve garde
//!
//! Elle mesure **le corps JSON des ROUTES montées** — `GET`/`PATCH
//! /system/config` et `POST …/indexer` —, jamais une constante. Un test qui
//! comparerait `PISTES_DEFAUT` à `100_000` ne garderait rien : il recopierait
//! la déclaration. Ce qui compte, c'est que la valeur **publiée** soit celle
//! que l'indexation **applique**.
//!
//! Le banc publie **six pistes** sous un axe unique, et le plafond est posé à
//! **deux** : la troncature est donc certaine et son chiffre connu. Un banc
//! plus petit que le plafond ne prouverait rien — c'est le piège de tout témoin
//! de borne.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`. Sans elle il ne serait JAMAIS
//! compilé, et la garde serait verte contre rien.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::routes::indexation_upnp::{
    CONTENEURS_DEFAUT, PISTES_DEFAUT, PROFONDEUR_DEFAUT, PROFONDEUR_PLAFOND_DUR, reglages,
};
use tune_server::state::AppState;

const UDN: &str = "uuid:258FC2D5-E2C3-B734-0-123456789abc";

/// Combien de pistes le banc publie. **Strictement plus que le plafond posé**,
/// sinon la troncature n'aurait pas lieu et l'épreuve serait verte pour rien.
const PISTES_DU_BANC: usize = 6;
/// Le plafond posé pendant l'épreuve.
const PLAFOND_POSE: usize = 2;

/// Le plancher du détecteur.
///
/// Trois choses qu'un remaniement distrait casserait sans que rien d'autre ne
/// rougisse : un banc retombé sous le plafond, un défaut de pistes revenu à la
/// valeur mal calée, et une profondeur dure descendue sous ce que le défaut
/// demande déjà.
#[test]
fn le_banc_et_les_defauts_tiennent_debout() {
    assert!(
        PISTES_DU_BANC > PLAFOND_POSE,
        "le banc ({PISTES_DU_BANC}) doit dépasser le plafond ({PLAFOND_POSE}), \
         sinon rien n'est tronqué et l'épreuve ne mesure rien"
    );
    assert!(
        PISTES_DEFAUT >= 100_000,
        "le défaut de pistes est retombé à {PISTES_DEFAUT} : 50 000 ne laissait \
         que 6 % de marge sur une bibliothèque de 47 056 pistes"
    );
    assert!(
        PROFONDEUR_PLAFOND_DUR > PROFONDEUR_DEFAUT,
        "le plafond DUR de profondeur doit rester au-dessus du défaut"
    );
}

// ── Le serveur ContentDirectory du banc ──────────────────────────────────────

fn enveloppe_soap(didl: &str, nombre: usize) -> axum::response::Response<String> {
    let echappe = didl
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    axum::response::Response::builder()
        .header("content-type", "text/xml")
        .body(format!(
            "<?xml version=\"1.0\"?><s:Envelope \
             xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <Result>{echappe}</Result><NumberReturned>{nombre}</NumberReturned>\
             <TotalMatches>{nombre}</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>"
        ))
        .unwrap()
}

fn didl_piste(index: usize, base: &str) -> String {
    format!(
        "<item id=\"piste-{index}\" parentID=\"axe\"><dc:title>Piste {index}</dc:title>\
         <upnp:artist>Artiste {index}</upnp:artist><upnp:album>Album {index}</upnp:album>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:03:0{index}.000\" \
         size=\"{}\">{base}/p{index}.flac</res></item>",
        1_000_000 + index
    )
}

/// Un axe unique, six pistes toutes distinctes : aucune n'est repliée par la
/// clé d'identité, le compte vu est donc bien le compte publié.
async fn lever_le_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse liée");
    let base = format!("http://{adresse}");
    let pour_le_service = base.clone();

    let app = Router::new().route(
        "/control",
        post(move |corps: String| {
            let base = pour_le_service.clone();
            async move {
                let object_id = corps
                    .split_once("<ObjectID>")
                    .and_then(|(_, r)| r.split_once("</ObjectID>"))
                    .map(|(v, _)| v.to_string())
                    .unwrap_or_default();
                let depart: usize = corps
                    .split_once("<StartingIndex>")
                    .and_then(|(_, r)| r.split_once("</StartingIndex>"))
                    .and_then(|(v, _)| v.trim().parse().ok())
                    .unwrap_or(0);
                if depart > 0 {
                    return enveloppe_soap("", 0);
                }
                match object_id.as_str() {
                    "0" => enveloppe_soap(
                        "<container id=\"axe\" parentID=\"0\" childCount=\"6\">\
                         <dc:title>Album</dc:title></container>",
                        1,
                    ),
                    "axe" => {
                        let didl: String =
                            (0..PISTES_DU_BANC).map(|i| didl_piste(i, &base)).collect();
                        enveloppe_soap(&didl, PISTES_DU_BANC)
                    }
                    _ => enveloppe_soap("", 0),
                }
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("{base}/control")
}

async fn inscrire(etat: &AppState, control_url: &str) {
    let info = tune_core::discovery::ssdp::MediaServerInfo {
        id: UDN.to_string(),
        name: "Banc UPnP".to_string(),
        manufacturer: "Illustrate Ltd".to_string(),
        model: "Asset UPnP Server".to_string(),
        location: control_url.to_string(),
        content_directory_url: control_url.to_string(),
        host: "127.0.0.1".to_string(),
        port: 0,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    };
    etat.media_servers
        .lock()
        .await
        .insert(UDN.to_string(), info);
}

// ── Plomberie HTTP ───────────────────────────────────────────────────────────

async fn lire(app: &Router, chemin: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(chemin).body(Body::empty()).unwrap()).await
}

async fn patcher(app: &Router, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::patch("/api/v1/system/config")
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn indexer(app: &Router) -> Value {
    let (statut, corps) = envoyer(
        app,
        Request::post(format!("/api/v1/network/media-servers/{UDN}/indexer"))
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "l'indexation répond : {corps}");
    assert_eq!(
        corps["indexe"],
        json!(true),
        "le serveur du banc doit être trouvé au registre : {corps}"
    );
    corps
}

async fn envoyer(app: &Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(requete)
        .await
        .expect("routeur en échec");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Toutes les phrases que la réponse d'indexation adresse à l'utilisateur.
fn phrases(corps: &Value) -> String {
    let reserves = corps["reserves"].as_array().cloned().unwrap_or_default();
    let mut tout: Vec<String> = reserves
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    if let Some(m) = corps["parcours"]["plafond"]["message"].as_str() {
        tout.push(m.to_string());
    }
    tout.join("\n")
}

// ── Les épreuves ─────────────────────────────────────────────────────────────

/// **1. Sur une base vierge, les trois plafonds sont PUBLIÉS, à leur défaut.**
///
/// La boucle générique de `GET /config` ne rend que les lignes qui EXISTENT :
/// un réglage jamais posé n'apparaîtrait pas, et l'écran n'aurait rien à
/// afficher tant que personne n'y aurait touché — c'est-à-dire au moment
/// exact où l'utilisateur a besoin de savoir sur quoi il est.
#[tokio::test(flavor = "multi_thread")]
async fn i4154_une_base_vierge_publie_les_trois_plafonds() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let app = tune_server::routes::router(etat);

    let (statut, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(statut, StatusCode::OK);

    assert_eq!(
        config[reglages::MAX_PISTES],
        json!(PISTES_DEFAUT),
        "le plafond de pistes doit être publié à son défaut — corps {config}"
    );
    assert_eq!(
        config[reglages::MAX_CONTENEURS],
        json!(CONTENEURS_DEFAUT),
        "le plafond de conteneurs doit être publié à son défaut — corps {config}"
    );
    assert_eq!(
        config[reglages::PROFONDEUR_MAX],
        json!(PROFONDEUR_DEFAUT),
        "la profondeur doit être publiée à son défaut — corps {config}"
    );
}

/// **2. Le réglage se pose, se SOUVIENT, et l'indexation l'applique.**
///
/// C'est la seule preuve qui compte : un réglage qui se relit mais que la
/// récolte ignore serait une case à cocher décorative.
#[tokio::test(flavor = "multi_thread")]
async fn i4154_le_plafond_regle_borne_reellement_la_recolte() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat);

    // Sans réglage, le banc entier rentre : le défaut ne tronque rien.
    let avant = indexer(&app).await;
    assert_eq!(
        avant["pistes"]["distinctes"],
        json!(PISTES_DU_BANC),
        "au défaut, les {PISTES_DU_BANC} pistes du banc doivent entrer — {avant}"
    );
    assert_eq!(
        avant["parcours"]["plafond_atteint"],
        Value::Null,
        "aucun plafond ne doit mordre au défaut — {avant}"
    );

    // On pose le plafond, et il se souvient.
    let (statut, reponse) = patcher(&app, json!({ reglages::MAX_PISTES: PLAFOND_POSE })).await;
    assert_eq!(statut, StatusCode::OK, "le PATCH est accepté : {reponse}");
    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[reglages::MAX_PISTES],
        json!(PLAFOND_POSE),
        "le plafond posé doit se relire — corps {config}"
    );

    // La passe suivante le subit, sans qu'on lui repasse quoi que ce soit.
    let apres = indexer(&app).await;
    assert_eq!(
        apres["parcours"]["plafond_atteint"],
        json!("pistes"),
        "le plafond réglé doit MORDRE — {apres}"
    );
    assert_eq!(
        apres["parcours"]["plafonds"][reglages::MAX_PISTES.trim_start_matches("upnp_index_")],
        json!(PLAFOND_POSE),
        "la réponse doit dire la borne EFFECTIVE de la passe — {apres}"
    );
}

/// **3. La troncature NOMME le réglage à relever.** Le cœur de #4154.
#[tokio::test(flavor = "multi_thread")]
async fn i4154_la_troncature_dit_quel_reglage_relever() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat);

    patcher(&app, json!({ reglages::MAX_PISTES: PLAFOND_POSE })).await;
    let corps = indexer(&app).await;

    let plafond = &corps["parcours"]["plafond"];
    assert_eq!(plafond["nature"], json!("pistes"), "— {corps}");
    assert_eq!(plafond["valeur"], json!(PLAFOND_POSE), "— {corps}");
    assert_eq!(
        plafond["reglage"],
        json!(reglages::MAX_PISTES),
        "la réponse doit nommer LE RÉGLAGE à relever, pas seulement la nature \
         du plafond — {corps}"
    );

    let dit = phrases(&corps);
    assert!(
        dit.contains(reglages::MAX_PISTES),
        "les phrases adressées à l'utilisateur doivent citer le réglage — {dit}"
    );
    assert!(
        dit.contains(&PLAFOND_POSE.to_string()),
        "…et le CHIFFRE auquel il a coupé, sinon on ne sait pas de combien le \
         relever — {dit}"
    );
    // La phrase d'avant #4154, celle qui n'aidait personne, ne doit plus être
    // la seule chose dite.
    assert!(
        !dit.contains("plafond « pistes » atteint : le parcours s'est arrêté"),
        "l'ancienne phrase muette ne doit plus être servie — {dit}"
    );
}

/// **4. Les trois natures de plafond ont chacune leur message.**
///
/// On ne règle pas le même problème selon qu'on a manqué de pistes, de
/// conteneurs ou de profondeur — et le message doit envoyer vers le bon
/// réglage. Ici, un plafond de CONTENEURS à 1 : la racine visitée, l'axe non.
#[tokio::test(flavor = "multi_thread")]
async fn i4154_le_plafond_de_conteneurs_a_son_propre_message() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat);

    patcher(&app, json!({ reglages::MAX_CONTENEURS: 1 })).await;
    let corps = indexer(&app).await;

    assert_eq!(
        corps["parcours"]["plafond"]["nature"],
        json!("conteneurs"),
        "— {corps}"
    );
    assert_eq!(
        corps["parcours"]["plafond"]["reglage"],
        json!(reglages::MAX_CONTENEURS),
        "un manque de CONTENEURS doit envoyer vers le réglage des conteneurs, \
         pas vers celui des pistes — {corps}"
    );
    let dit = phrases(&corps);
    assert!(
        !dit.contains(reglages::MAX_PISTES),
        "…et surtout PAS vers celui des pistes — {dit}"
    );
}

/// **5. « Sans limite » : `null` fait l'aller-retour, et ne tronque plus.**
#[tokio::test(flavor = "multi_thread")]
async fn i4154_sans_limite_fait_l_aller_retour_et_ne_tronque_pas() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat);

    patcher(&app, json!({ reglages::MAX_PISTES: PLAFOND_POSE })).await;
    let (statut, reponse) = patcher(&app, json!({ reglages::MAX_PISTES: Value::Null })).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "« sans limite » est accepté : {reponse}"
    );

    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[reglages::MAX_PISTES],
        Value::Null,
        "« sans limite » doit se relire comme `null`, et jamais comme \
         18 446 744 073 709 551 615 — corps {config}"
    );

    let corps = indexer(&app).await;
    assert_eq!(
        corps["pistes"]["distinctes"],
        json!(PISTES_DU_BANC),
        "sans limite, le banc entier doit entrer — {corps}"
    );
    assert_eq!(
        corps["parcours"]["plafond_atteint"],
        Value::Null,
        "…et aucun plafond ne doit mordre — {corps}"
    );
    // 🔴 Sans cette ligne, l'épreuve serait VERTE pour la mauvaise raison.
    //
    // La contre-épreuve l'a montré : correctif retiré — donc réglages ignorés
    // et défaut en dur appliqué —, un banc de six pistes ne heurte pas
    // davantage un plafond de 50 000. « Rien n'a été tronqué » ne distingue
    // donc PAS « sans limite appliqué » de « réglage jamais lu ». La borne
    // EFFECTIVE que la passe annonce, elle, les sépare.
    assert_eq!(
        corps["parcours"]["plafonds"]["max_pistes"],
        Value::Null,
        "la passe doit annoncer qu'elle a tourné SANS LIMITE, pas sous un \
         défaut qu'on ne lui a pas demandé — {corps}"
    );
}

/// **6. « Sans limite » sur la PROFONDEUR n'existe pas.**
///
/// Le parcours retient les conteneurs déjà vus, ce qui ferme le cycle
/// ordinaire. Mais l'en-tête du module d'indexation documente qu'un
/// `ObjectID` peut CHANGER d'une visite à l'autre : un serveur qui en frappe un
/// neuf à chaque fois donnerait une clé différente à chaque descente, et le
/// parcours ne s'arrêterait jamais. Le plafond dur borne cette boucle-là, pas
/// un catalogue réel — Asset demande trois niveaux.
#[tokio::test(flavor = "multi_thread")]
async fn i4154_la_profondeur_garde_un_plafond_dur() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let app = tune_server::routes::router(etat);

    let (statut, reponse) = patcher(&app, json!({ reglages::PROFONDEUR_MAX: Value::Null })).await;
    assert_eq!(statut, StatusCode::OK, "accepté : {reponse}");
    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[reglages::PROFONDEUR_MAX],
        json!(PROFONDEUR_PLAFOND_DUR),
        "« sans limite » sur la profondeur doit se relire au plafond DUR, pas \
         comme une descente infinie — corps {config}"
    );

    // Une valeur au-delà du plafond dur est bornée, pas refusée : l'intention
    // « le plus profond possible » est légitime.
    patcher(&app, json!({ reglages::PROFONDEUR_MAX: 10_000 })).await;
    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[reglages::PROFONDEUR_MAX],
        json!(PROFONDEUR_PLAFOND_DUR),
        "une profondeur démesurée est ramenée au plafond dur — corps {config}"
    );
}

/// **7. Une valeur illisible est REFUSÉE, jamais devinée.**
///
/// Sans ce refus, `{"upnp_index_max_pistes": "beaucoup"}` tomberait dans la
/// boucle d'écriture générique, installerait une ligne morte, répondrait
/// `{"ok": true}` — et l'indexation retomberait sur le défaut. L'utilisateur
/// croirait avoir levé un plafond qui n'a pas bougé : le défaut muet de #4154,
/// déplacé d'un cran.
#[tokio::test(flavor = "multi_thread")]
async fn i4154_une_valeur_illisible_est_refusee_pas_devinee() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let app = tune_server::routes::router(etat);

    let (statut, reponse) = patcher(&app, json!({ reglages::MAX_PISTES: "beaucoup" })).await;
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "une valeur illisible doit être refusée : {reponse}"
    );

    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[reglages::MAX_PISTES],
        json!(PISTES_DEFAUT),
        "…et RIEN ne doit avoir été écrit — corps {config}"
    );
}
