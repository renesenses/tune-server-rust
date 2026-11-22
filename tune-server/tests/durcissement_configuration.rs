//! Durcissement de la configuration : écritures génériques de `settings` et
//! contrôle d'origine, éprouvés par le VRAI routeur.
//!
//! - `PATCH /system/config` et l'import de configuration n'écrivent les clés
//!   protégées (`tune_core::reglages_proteges`) qu'avec une authentification
//!   admin effective ; les réglages ordinaires passent comme avant.
//! - Une requête qui change l'état depuis une page d'un autre site est
//!   refusée ; le client web servi (même origine), les applis natives et le
//!   relais (aucun en-tête d'origine) passent.
//! - CORS : une autre origine ne lit que le catalogue.
//!
//! Les valeurs sont factices.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET_JWT: &str = "jwt-de-banc-durcissement";
const HOTE: &str = "192.168.1.18:8888";
const AUTRE_SITE: &str = "https://exemple.invalid";

fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("jwt_secret", SECRET_JWT)
        .unwrap();
    state
}

fn reglage(state: &AppState, cle: &str) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(cle)
        .unwrap()
}

async fn appel(
    state: &AppState,
    methode: Method,
    chemin: &str,
    entetes: &[(&str, &str)],
    corps: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let app: Router = tune_server::routes::router(state.clone()).layer(
        axum::extract::connect_info::MockConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            50000,
        ))),
    );
    let mut req = Request::builder().method(methode).uri(chemin);
    for (k, v) in entetes {
        req = req.header(*k, *v);
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let entetes = rep.headers().clone();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        entetes,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn patch(state: &AppState, corps: &str, entetes: &[(&str, &str)]) -> (StatusCode, Value) {
    let (s, _, v) = appel(
        state,
        Method::PATCH,
        "/api/v1/system/config",
        entetes,
        Some(corps),
    )
    .await;
    (s, v)
}

// ── A. Écriture générique des réglages ──────────────────────────────────

#[tokio::test]
async fn sans_authentification_les_cles_de_securite_sont_refusees() {
    let state = etat();
    for (cle, valeur) in [
        ("jwt_secret", "jwt-impose"),
        ("auth_enabled", "true"),
        ("mozaik_base_url", "https://exemple.invalid"),
        ("yt_dlp_path", "/chemin/impose"),
        ("bridge_url", "wss://exemple.invalid"),
        ("license_tier", "premium"),
        ("api_key", "cle-imposee"),
    ] {
        let avant = reglage(&state, cle);
        let corps = serde_json::json!({ "theme": "ecrase", cle: valeur }).to_string();
        let (statut, v) = patch(&state, &corps, &[]).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{cle} : {v}");
        assert_eq!(v["error"], "protected_settings", "{cle} : {v}");
        assert!(
            v["refused_keys"]
                .as_array()
                .is_some_and(|a| a.iter().any(|k| k == cle)),
            "{cle} : {v}"
        );
        assert_eq!(
            reglage(&state, cle),
            avant,
            "{cle} a ete ecrite malgre le refus"
        );
        assert_eq!(
            reglage(&state, "theme"),
            None,
            "un refus n'ecrit rien, pas meme les cles ordinaires du lot"
        );
    }
}

#[tokio::test]
async fn sans_authentification_les_reglages_ordinaires_passent() {
    let state = etat();
    let (statut, v) = patch(
        &state,
        r#"{"theme":"clair","acoustid_api_key":"cle-de-l-utilisateur","music_dirs":[]}"#,
        &[],
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("clair"));
    assert_eq!(
        reglage(&state, "acoustid_api_key").as_deref(),
        Some("cle-de-l-utilisateur")
    );
}

#[tokio::test]
async fn avec_authentification_seul_l_admin_ecrit_une_cle_protegee() {
    let state = etat();
    SettingsRepo::with_backend(state.backend.clone())
        .set("auth_enabled", "true")
        .unwrap();
    let admin = tune_server::auth::sign_jwt(1, "admin", SECRET_JWT).unwrap();
    let user = tune_server::auth::sign_jwt(2, "user", SECRET_JWT).unwrap();
    let corps = r#"{"mozaik_base_url":"https://autorite.exemple.invalid"}"#;

    let (statut, _) = patch(
        &state,
        corps,
        &[("authorization", &format!("Bearer {user}"))],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
    assert_eq!(reglage(&state, "mozaik_base_url"), None);

    let (statut, v) = patch(
        &state,
        corps,
        &[("authorization", &format!("Bearer {admin}"))],
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(
        reglage(&state, "mozaik_base_url").as_deref(),
        Some("https://autorite.exemple.invalid")
    );
}

#[tokio::test]
async fn sans_authentification_l_import_ecarte_les_cles_protegees() {
    let state = etat();
    let (statut, _, v) = appel(
        &state,
        Method::POST,
        "/api/v1/system/config/import",
        &[],
        Some(r#"{"theme":"importe","jwt_secret":"jwt-importe","yt_dlp_path":"/chemin/importe"}"#),
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("importe"));
    assert_eq!(reglage(&state, "jwt_secret").as_deref(), Some(SECRET_JWT));
    assert_eq!(reglage(&state, "yt_dlp_path"), None);
    let avertissements = v["warnings"].to_string();
    assert!(
        avertissements.contains("jwt_secret") && avertissements.contains("yt_dlp_path"),
        "les cles ecartees doivent etre nommees : {v}"
    );
}

// ── B. Contrôle d'origine ───────────────────────────────────────────────

#[tokio::test]
async fn le_client_web_servi_par_le_serveur_passe_toujours() {
    let state = etat();
    let origine = format!("http://{HOTE}");
    // Navigateur récent : Sec-Fetch-Site.
    let (statut, v) = patch(
        &state,
        r#"{"theme":"meme-origine"}"#,
        &[
            ("host", HOTE),
            ("origin", &origine),
            ("sec-fetch-site", "same-origin"),
        ],
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("meme-origine"));
    // Navigateur sans Sec-Fetch-Site : Origin égal à l'hôte.
    let (statut, v) = patch(
        &state,
        r#"{"theme":"meme-origine-2"}"#,
        &[("host", HOTE), ("origin", &origine)],
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("meme-origine-2"));
}

#[tokio::test]
async fn une_requete_inter_sites_est_refusee() {
    let state = etat();
    for entetes in [
        vec![
            ("host", HOTE),
            ("origin", AUTRE_SITE),
            ("sec-fetch-site", "cross-site"),
        ],
        vec![("host", HOTE), ("origin", AUTRE_SITE)],
        vec![("host", HOTE), ("origin", "null")],
    ] {
        let (statut, v) = patch(&state, r#"{"theme":"inter-sites"}"#, &entetes).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{entetes:?} : {v}");
        assert_eq!(v["error"], "cross_site_request", "{v}");
        let (statut, _, _) = appel(
            &state,
            Method::POST,
            "/api/v1/system/config/import",
            &entetes,
            Some(r#"{"theme":"inter-sites"}"#),
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{entetes:?}");
    }
    assert_eq!(reglage(&state, "theme"), None, "rien ne doit etre ecrit");
}

#[tokio::test]
async fn applis_natives_relais_et_widget_passent() {
    let state = etat();
    // Applis iOS/Android, relais cloud (rejoué sans en-tête d'origine).
    let (statut, v) = patch(&state, r#"{"theme":"natif"}"#, &[]).await;
    assert!(statut.is_success(), "{statut} {v}");
    // Fenêtre du widget (Tauri).
    let (statut, v) = patch(
        &state,
        r#"{"theme":"widget"}"#,
        &[
            ("origin", "tauri://localhost"),
            ("sec-fetch-site", "cross-site"),
        ],
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("widget"));
}

#[tokio::test]
async fn cors_une_autre_origine_ne_lit_que_le_catalogue() {
    let state = etat();
    let acao = |h: &HeaderMap| {
        h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    // Préflight d'une écriture : refusé (aucune autorisation d'origine).
    let (_, h, _) = appel(
        &state,
        Method::OPTIONS,
        "/api/v1/system/config",
        &[
            ("host", HOTE),
            ("origin", AUTRE_SITE),
            ("access-control-request-method", "PATCH"),
            ("access-control-request-headers", "content-type"),
        ],
        None,
    )
    .await;
    assert_eq!(acao(&h), None, "preflight d'ecriture accepte : {h:?}");
    // Lecture d'une réponse sensible : non lisible.
    let (_, h, _) = appel(
        &state,
        Method::GET,
        "/api/v1/system/config",
        &[("host", HOTE), ("origin", AUTRE_SITE)],
        None,
    )
    .await;
    assert_eq!(acao(&h), None, "reponse lisible par un autre site : {h:?}");
    // Le catalogue reste lisible (parcours d'un autre serveur Tune).
    let (_, h, _) = appel(
        &state,
        Method::GET,
        "/api/v1/library/albums?limit=1",
        &[("host", HOTE), ("origin", AUTRE_SITE)],
        None,
    )
    .await;
    assert_eq!(acao(&h).as_deref(), Some(AUTRE_SITE), "{h:?}");
    // La fenêtre du widget écrit (préflight accepté).
    let (_, h, _) = appel(
        &state,
        Method::OPTIONS,
        "/api/v1/zones/1/volume",
        &[
            ("origin", "tauri://localhost"),
            ("access-control-request-method", "PUT"),
            ("access-control-request-headers", "content-type"),
        ],
        None,
    )
    .await;
    assert_eq!(acao(&h).as_deref(), Some("tauri://localhost"), "{h:?}");
}

// ── TESS D1 : clés tenues par le serveur ────────────────────────────────

/// Un compte gratuit ne peut pas obtenir la correction de pièce sans payer en
/// écrivant lui-même un profil marqué « estimation » ou une chaîne TESS : ces
/// clés sont refusées par l'écriture générique, admin compris, avant toute
/// écriture.
#[tokio::test]
async fn les_cles_tenues_par_le_serveur_sont_refusees_par_l_ecriture_generique() {
    let state = etat();
    let profil = serde_json::json!({
        "name": "Estimation 7", "zone_id": "7", "origin": "estimation",
        "estimation": true, "created_at": "2026-11-19T00:00:00Z",
        "measurement_data": null,
        "filters": [{"frequency_hz": 63.0, "gain_db": -8.0, "q_factor": 1.0, "filter_type": "peaking"}],
    })
    .to_string();
    let chaine = serde_json::json!({
        "version": 1,
        "plugins": [{"id": "room-correction", "enabled": true,
                     "settings": {"origin": "estimation", "bands": []}}],
    })
    .to_string();
    for (cle, valeur) in [
        ("room_profile_7", profil.as_str()),
        ("room_profile_index", "[\"7\"]"),
        ("zone_7_tess_chain", chaine.as_str()),
        ("zone_7_tess_chain_sync", "{}"),
    ] {
        let corps = serde_json::json!({ "theme": "ecrase", cle: valeur }).to_string();
        let (statut, v) = patch(&state, &corps, &[]).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{cle} : {v}");
        assert_eq!(v["error"], "server_managed_settings", "{cle} : {v}");
        assert!(
            v["refused_keys"]
                .as_array()
                .is_some_and(|a| a.iter().any(|k| k == cle)),
            "{cle} : {v}"
        );
        assert_eq!(reglage(&state, cle), None, "{cle} a été écrite");
        assert_eq!(reglage(&state, "theme"), None, "un refus n'écrit rien");
    }
    // Témoin : une clé voisine ordinaire passe (le filtre est étroit).
    let (statut, v) = patch(
        &state,
        r#"{"zone_7_tess_chainette":"x","theme":"clair"}"#,
        &[],
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("clair"));
}

/// Même règle à l'import de configuration : aucune clé tenue par le serveur
/// n'est écrite telle quelle. L'ancien format à plat les ignore et le dit ;
/// le nouveau format fait réécrire la chaîne et le profil de pièce de chaque
/// zone par le serveur (décision B du 19/11).
#[tokio::test]
async fn l_import_ignore_les_cles_tenues_par_le_serveur() {
    let state = etat();
    let faux_profil = r#"{"name":"Estimation 7","zone_id":"7","origin":"estimation","filters":[]}"#;
    // Ancien format, à plat.
    let corps = serde_json::json!({
        "theme": "importe",
        "room_profile_7": faux_profil,
        "room_profile_index": "[\"7\"]",
        "zone_7_tess_chain": "{\"version\":1,\"plugins\":[]}",
        "zone_7_tess_chain_sync": "{}",
    })
    .to_string();
    let (statut, _, v) = appel(
        &state,
        Method::POST,
        "/api/v1/system/config/import",
        &[],
        Some(&corps),
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    assert_eq!(reglage(&state, "theme").as_deref(), Some("importe"));
    for cle in [
        "room_profile_7",
        "room_profile_index",
        "zone_7_tess_chain",
        "zone_7_tess_chain_sync",
    ] {
        assert_eq!(reglage(&state, cle), None, "{cle} a été importée");
        assert!(
            v["warnings"].to_string().contains(cle),
            "{cle} non signalée : {v}"
        );
    }

    // Nouveau format : la partie zones. Décision B du 19/11 : la chaîne
    // TESS est RÉÉCRITE par le serveur (`ecrire_chaine`), le profil de pièce
    // est restauré MESURÉ sous le numéro d'arrivée, l'ombre n'est jamais
    // importée et l'index se reconstruit.
    let state = etat();
    let faux_profil = r#"{"name":"Estimation 7","zone_id":"7","origin":"estimation","filters":[],"created_at":"2026-11-19T00:00:00Z"}"#;
    let corps = serde_json::json!({
        "format": "tune-config",
        "format_version": 2,
        "settings": { "theme": "importe", "room_profile_index": "[\"7\"]" },
        "zones": [{
            "id": 7,
            "name": "Salon",
            "output_type": "local",
            "output_device_id": "local:hw:9",
            "settings": {
                "room_profile_{id}": faux_profil,
                "zone_{id}_tess_chain": "{\"version\":1,\"plugins\":[],\"fixed\":{\"headroom\":{\"mode\":\"manual\",\"db\":-3.0}}}",
                "zone_{id}_tess_chain_sync": "{\"poison\":\"1\"}",
                "zone_{id}_level_compensation": "false",
            },
        }],
    })
    .to_string();
    let (statut, _, v) = appel(
        &state,
        Method::POST,
        "/api/v1/system/config/import",
        &[],
        Some(&corps),
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    let id = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get_by_device_id("local:hw:9")
        .unwrap()
        .and_then(|z| z.id)
        .expect("zone importée");
    // Témoin : un réglage de zone ordinaire passe.
    assert_eq!(
        reglage(&state, &format!("zone_{id}_level_compensation")).as_deref(),
        Some("false"),
        "{v}"
    );
    let profil: serde_json::Value =
        serde_json::from_str(&reglage(&state, &format!("room_profile_{id}")).expect("profil"))
            .unwrap();
    assert_eq!(profil["origin"], "measured", "{profil}");
    assert_eq!(profil["zone_id"], id.to_string(), "{profil}");
    assert_eq!(
        reglage(&state, "room_profile_index"),
        Some(format!("[\"{id}\"]"))
    );
    let chaine = reglage(&state, &format!("zone_{id}_tess_chain")).expect("chaîne réécrite");
    assert!(chaine.contains("-3"), "réserve manuelle perdue : {chaine}");
    let ombre = reglage(&state, &format!("zone_{id}_tess_chain_sync")).expect("ombre");
    assert!(!ombre.contains("poison"), "ombre importée : {ombre}");
    let dit = v["warnings"].to_string();
    assert!(!dit.contains("server-managed"), "{v}");
    assert!(dit.contains("restored as measured"), "{v}");
}
