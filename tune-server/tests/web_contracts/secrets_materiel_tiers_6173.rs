//! Reliquat de #1897 (#6173) : les routes qui manipulent des SECRETS, du
//! MATÉRIEL ou un SERVICE TIERS, confrontées à la carte du client web.
//!
//! Les vagues précédentes les avaient écartées faute de pouvoir les jouer sans
//! compte réel, sans appareil ou sans réseau. Ici, chacune passe par le VRAI
//! routeur, contre un faux local :
//!
//! - **secrets** : un compte YouTube Music restauré avec des jetons FACTICES
//!   (aucun secret réel ; ses bases pointent sur `127.0.0.1:9`, où rien ne
//!   répond). On vérifie les champs que le web lit, et que la réponse ne
//!   renvoie jamais un jeton, et que la déconnexion l'efface du stockage ;
//! - **matériel** : un faux renderer DLNA local (ConnectionManager qui répond
//!   à `GetProtocolInfo`), le registre des sorties et la liste des appareils
//!   ignorés ;
//! - **service tiers** : un faux serveur Tune pair, sur `127.0.0.1`.
//!
//! Aucun appel ne quitte la machine : la télémétrie (remontée du catalogue
//! d'appareils après une sonde réussie) est coupée par son réglage.
//!
//! Restent hors de portée, et le disent : l'appairage AirPlay 2 (il exige une
//! vraie sortie `Airplay2Output` et son démon), le montage SMB (il exige
//! `mount` et des droits système), l'activation de licence et la connexion
//! SSO (serveur mozaiklabs), et la connexion par identifiants des services
//! (Qobuz, TIDAL, Deezer… : leurs bases ne sont pas injectables).
use super::{CARTE_WEB, CarteContrats, mutation_json, respecte_tous_les_contrats};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::streaming::traits::StreamingService;

/// Jetons FACTICES : aucun n'a jamais été émis par un service.
const ACCES_FACTICE: &str = "jeton-factice-6173-acces";
const RAFRAICHISSEMENT_FACTICE: &str = "jeton-factice-6173-rafraichissement";

fn carte() -> CarteContrats {
    serde_json::from_str(CARTE_WEB).expect("carte contrat web")
}

fn etat() -> tune_server::state::AppState {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    // Une sonde DLNA réussie remonte ses capacités au catalogue communautaire :
    // coupé, pour qu'aucun test ne parte sur le réseau.
    SettingsRepo::with_backend(etat.backend.clone())
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
        .expect("telemetrie coupee");
    etat
}

/// Un YouTube Music « connecté » avec des jetons factices, sans réseau.
async fn youtube_connecte(etat: &tune_server::state::AppState) {
    let mut yt = tune_core::streaming::youtube::YouTubeService::avec_bases(
        "http://127.0.0.1:9/youtubei/v1",
        "http://127.0.0.1:9/youtube/v3",
    );
    assert!(
        yt.restore_tokens(&json!({
            "access_token": ACCES_FACTICE,
            "refresh_token": RAFRAICHISSEMENT_FACTICE,
            "email": "temoin@example.invalid",
        })),
        "restauration des jetons factices"
    );
    etat.services.lock().await.register(Box::new(yt));
}

fn sans_jeton(payload: &Value, contexte: &str) {
    let texte = payload.to_string();
    for jeton in [ACCES_FACTICE, RAFRAICHISSEMENT_FACTICE] {
        assert!(
            !texte.contains(jeton),
            "{contexte} : la reponse renvoie un jeton ; payload={payload}"
        );
    }
}

/// Secrets — l'authentification d'un compte déjà connecté rend ce que le web
/// lit (`authenticated`), et jamais les jetons qu'elle tient.
#[tokio::test]
async fn l_authentification_d_un_service_rend_son_etat_sans_ses_jetons_6173() {
    let carte = carte();
    let etat = etat();
    youtube_connecte(&etat).await;
    let app = tune_server::routes::router(etat);
    let payload = mutation_json(
        &app,
        Method::POST,
        "/api/v1/streaming/youtube/auth",
        json!({"device_flow": true}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "POST", "/streaming/{}/auth", &payload)
        .unwrap_or_else(|e| panic!("{e}; payload={payload}"));
    assert_eq!(payload["authenticated"], true, "payload={payload}");
    sans_jeton(&payload, "POST /streaming/youtube/auth");
}

/// Secrets — la déconnexion dit `disconnected` (le champ que lit
/// `disconnectStreaming`), et les jetons quittent le stockage : un
/// redémarrage ne doit pas reconnecter le compte.
#[tokio::test]
async fn la_deconnexion_d_un_service_efface_ses_jetons_et_le_dit_au_web_6173() {
    let carte = carte();
    let etat = etat();
    youtube_connecte(&etat).await;
    let reglages = SettingsRepo::with_backend(etat.backend.clone());
    etat.services
        .lock()
        .await
        .save_all_tokens(&etat.backend)
        .await;
    let avant = reglages
        .get("auth_tokens_youtube")
        .unwrap()
        .unwrap_or_default();
    assert!(
        avant.contains(ACCES_FACTICE),
        "le temoin exige des jetons ranges avant la deconnexion : {avant}"
    );
    let app = tune_server::routes::router(etat);
    let payload = mutation_json(
        &app,
        Method::POST,
        "/api/v1/streaming/youtube/disconnect",
        json!({}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "POST", "/streaming/{}/disconnect", &payload)
        .unwrap_or_else(|e| panic!("{e}; payload={payload}"));
    assert_eq!(payload["disconnected"], true, "payload={payload}");
    sans_jeton(&payload, "POST /streaming/youtube/disconnect");
    let apres = reglages
        .get("auth_tokens_youtube")
        .unwrap()
        .unwrap_or_default();
    for jeton in [ACCES_FACTICE, RAFRAICHISSEMENT_FACTICE] {
        assert!(
            !apres.contains(jeton),
            "jeton encore stocke apres deconnexion : {apres}"
        );
    }
}

/// La case « Actif » d'un service : le web lit `status`.
#[tokio::test]
async fn activer_et_desactiver_un_service_rendent_le_statut_lu_par_le_web_6173() {
    let carte = carte();
    let etat = etat();
    youtube_connecte(&etat).await;
    let app = tune_server::routes::router(etat);
    for (geste, attendu) in [("disable", "disabled"), ("enable", "enabled")] {
        let payload = mutation_json(
            &app,
            Method::POST,
            &format!("/api/v1/streaming/youtube/{geste}"),
            json!({}),
            StatusCode::OK,
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        respecte_tous_les_contrats(
            &carte,
            "POST",
            &format!("/streaming/{{}}/{geste}"),
            &payload,
        )
        .unwrap_or_else(|e| panic!("{e}; payload={payload}"));
        assert_eq!(payload["status"], attendu, "payload={payload}");
    }
}

/// Faux renderer DLNA : un ConnectionManager qui annonce `sink`.
async fn faux_renderer(sink: &'static str) -> String {
    use axum::routing::post;
    let corps = format!(
        concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
            "<s:Body><u:GetProtocolInfoResponse><Source></Source><Sink>{}</Sink>",
            "</u:GetProtocolInfoResponse></s:Body></s:Envelope>"
        ),
        sink
    );
    let app = axum::Router::new().route(
        "/cm",
        post(move || {
            let corps = corps.clone();
            async move { ([(axum::http::header::CONTENT_TYPE, "text/xml")], corps) }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port local");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move { axum::serve(ecoute, app).await.expect("faux renderer") });
    format!("http://{adresse}")
}

const RENDERER: &str = "uuid:temoin-6173-renderer";

async fn zone_dlna(etat: &tune_server::state::AppState, base: &str) -> i64 {
    let sortie = tune_core::outputs::dlna::DlnaOutput::new(
        "Renderer temoin".into(),
        RENDERER.into(),
        base.into(),
        format!("{base}/avt"),
        format!("{base}/rc"),
        Some(format!("{base}/cm")),
    );
    etat.outputs.lock().await.register(Box::new(sortie));
    ZoneRepo::with_backend(etat.backend.clone())
        .create("Zone DLNA temoin", Some("dlna"), Some(RENDERER))
        .expect("zone dlna")
}

/// Matériel — « Vérifier le renderer » sonde le vrai chemin SOAP et rend
/// `probed` et les formats annoncés.
#[tokio::test]
async fn la_sonde_du_renderer_rend_ses_capacites_au_web_6173() {
    let carte = carte();
    let etat = etat();
    let base = faux_renderer("http-get:*:audio/flac:*,http-get:*:audio/L24:*").await;
    let zone = zone_dlna(&etat, &base).await;
    let app = tune_server::routes::router(etat);
    let payload = mutation_json(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{zone}/renderer-capabilities"),
        json!({}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "POST", "/zones/{}/renderer-capabilities", &payload)
        .unwrap_or_else(|e| panic!("{e}; payload={payload}"));
    assert_eq!(payload["probed"], true, "payload={payload}");
    assert_eq!(payload["flac"], true, "payload={payload}");
    assert_eq!(payload["lpcm24"], true, "payload={payload}");
    assert_eq!(payload["mp3"], false, "payload={payload}");
}

/// Matériel — ignorer un appareil masque sa zone ; ne plus l'ignorer la rend.
#[tokio::test]
async fn ignorer_puis_liberer_un_appareil_rend_les_champs_lus_par_le_web_6173() {
    let carte = carte();
    let etat = etat();
    let base = faux_renderer("http-get:*:audio/flac:*").await;
    let zone = zone_dlna(&etat, &base).await;
    let app = tune_server::routes::router(etat);
    let ignore = mutation_json(
        &app,
        Method::POST,
        &format!("/api/v1/devices/{RENDERER}/ignore"),
        json!({}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "POST", "/devices/{}/ignore", &ignore)
        .unwrap_or_else(|e| panic!("{e}; payload={ignore}"));
    assert_eq!(ignore["hidden_zone_ids"], json!([zone]), "payload={ignore}");
    let libere = mutation_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/devices/{RENDERER}/ignore"),
        json!({}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "DELETE", "/devices/{}/ignore", &libere)
        .unwrap_or_else(|e| panic!("{e}; payload={libere}"));
    assert!(
        libere["released"].as_array().is_some_and(|l| !l.is_empty()),
        "payload={libere}"
    );
}

/// Matériel — vider la liste des appareils retire les sorties enregistrées.
#[tokio::test]
async fn vider_les_appareils_compte_les_sorties_retirees_6173() {
    let carte = carte();
    let etat = etat();
    let base = faux_renderer("http-get:*:audio/flac:*").await;
    zone_dlna(&etat, &base).await;
    let sorties = etat.outputs.clone();
    let app = tune_server::routes::router(etat);
    let payload = mutation_json(
        &app,
        Method::POST,
        "/api/v1/devices/clear",
        json!({}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "POST", "/devices/clear", &payload)
        .unwrap_or_else(|e| panic!("{e}; payload={payload}"));
    assert!(
        payload["cleared"].as_u64().unwrap_or(0) >= 1,
        "payload={payload}"
    );
    assert!(
        sorties.lock().await.get(RENDERER).is_none(),
        "la sortie du renderer est encore enregistree"
    );
}

/// Faux serveur Tune pair : `GET /api/v1/system/peer-info`.
async fn faux_pair() -> u16 {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/api/v1/system/peer-info",
        get(|| async {
            axum::Json(json!({
                "name": "Pair temoin",
                "version": "0.0.0-temoin",
                "tracks": 12,
                "zones": 2,
            }))
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port local");
    let port = ecoute.local_addr().expect("adresse locale").port();
    tokio::spawn(async move { axum::serve(ecoute, app).await.expect("faux pair") });
    port
}

/// Service tiers — ajouter un serveur pair interroge l'autre serveur et rend
/// la fiche que le web lit (`TunePeer`) ; un pair muet est refusé (502).
#[tokio::test]
async fn ajouter_un_serveur_pair_rend_sa_fiche_et_refuse_un_pair_muet_6173() {
    let carte = carte();
    let etat = etat();
    let port = faux_pair().await;
    let app = tune_server::routes::router(etat);
    let fiche = mutation_json(
        &app,
        Method::POST,
        "/api/v1/system/peers",
        json!({"host": "127.0.0.1", "port": port}),
        StatusCode::OK,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "POST", "/system/peers", &fiche)
        .unwrap_or_else(|e| panic!("{e}; payload={fiche}"));
    assert_eq!(fiche["name"], "Pair temoin", "payload={fiche}");
    assert_eq!(fiche["tracks"], 12, "payload={fiche}");
    let muet = mutation_json(
        &app,
        Method::POST,
        "/api/v1/system/peers",
        json!({"host": "127.0.0.1", "port": 9}),
        StatusCode::BAD_GATEWAY,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(muet["error"], "peer_unreachable", "payload={muet}");
}
