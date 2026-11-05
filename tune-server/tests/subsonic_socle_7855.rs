//! #7855 — le socle Subsonic / OpenSubsonic (lot 4, étape 1 sur 3).
//!
//! Bout en bout, par le vrai routeur du serveur : le réglage « Accès
//! Subsonic », la licence Premium, les clés des clients et la garde de
//! `/rest`. Chaque témoin lit le code HTTP ET le corps rendu au client.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

fn banc() -> (AppState, axum::Router) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

/// Un serveur Premium, accès Subsonic allumé par sa route, et une clé créée.
async fn banc_ouvert() -> (AppState, axum::Router, String, String) {
    let (state, app) = banc();
    state.license.set_account_premium(true, None).await;
    let (code, _) = json_de(
        &app,
        "PUT",
        "/api/v1/subsonic/reglage",
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, corps) = json_de(
        &app,
        "POST",
        "/api/v1/subsonic/cles",
        json!({"nom": "Symfonium"}),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{corps}");
    let cle = corps["cle"].as_str().unwrap().to_string();
    let id = corps["id"].as_str().unwrap().to_string();
    (state, app, cle, id)
}

async fn brut(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, String, String) {
    let mut req = Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let type_ = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, type_, String::from_utf8_lossy(&bytes).into_owned())
}

async fn json_de(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let (s, _, texte) = brut(app, methode, chemin, Some(corps)).await;
    (s, serde_json::from_str(&texte).unwrap_or(json!(null)))
}

async fn get_json(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let (s, _, texte) = brut(app, "GET", chemin, None).await;
    (s, serde_json::from_str(&texte).unwrap_or(json!(null)))
}

/// La réponse Subsonic en JSON, son enveloppe ouverte.
async fn rest_json(app: &axum::Router, chemin: &str) -> Value {
    let (s, _, texte) = brut(app, "GET", chemin, None).await;
    assert_eq!(s, StatusCode::OK, "{chemin} : {texte}");
    let v: Value =
        serde_json::from_str(&texte).unwrap_or_else(|_| panic!("{chemin} : pas du JSON : {texte}"));
    v["subsonic-response"].clone()
}

fn code_d_erreur(r: &Value) -> i64 {
    assert_eq!(r["status"], "failed", "refus attendu, reçu : {r}");
    r["error"]["code"].as_i64().unwrap()
}

#[tokio::test]
async fn reglage_eteint_rest_repond_404_7855() {
    let (state, app) = banc();
    state.license.set_account_premium(true, None).await;
    let (code, corps) = get_json(&app, "/api/v1/subsonic/reglage").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        corps["enabled"], false,
        "l'accès Subsonic doit être éteint par défaut"
    );

    for chemin in [
        "/rest/ping",
        "/rest/ping.view?f=json",
        "/rest/inconnue",
        "/rest/getUser?apiKey=x",
    ] {
        let (s, _, texte) = brut(&app, "GET", chemin, None).await;
        assert_eq!(
            s,
            StatusCode::NOT_FOUND,
            "{chemin} doit répondre 404 réglage éteint, reçu {s} : {texte}"
        );
        assert!(
            !texte.contains("<html"),
            "{chemin} ne doit pas tomber dans le repli statique"
        );
    }
}

#[tokio::test]
async fn sans_premium_le_reglage_et_les_cles_sont_refuses_7855() {
    let (state, app) = banc();
    let (code, corps) = json_de(
        &app,
        "PUT",
        "/api/v1/subsonic/reglage",
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(code, StatusCode::PAYMENT_REQUIRED, "{corps}");
    assert_eq!(corps["code"], "subsonic");
    let (code, _) = json_de(
        &app,
        "POST",
        "/api/v1/subsonic/cles",
        json!({"nom": "DSub"}),
    )
    .await;
    assert_eq!(code, StatusCode::PAYMENT_REQUIRED);

    // Le réglage allumé puis la licence perdue : /rest refuse avec un code
    // Subsonic, même avec une clé valide.
    state.license.set_account_premium(true, None).await;
    let (_, corps) = json_de(
        &app,
        "PUT",
        "/api/v1/subsonic/reglage",
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(corps["enabled"], true);
    let (_, corps) = json_de(
        &app,
        "POST",
        "/api/v1/subsonic/cles",
        json!({"nom": "DSub"}),
    )
    .await;
    let cle = corps["cle"].as_str().unwrap().to_string();
    state.license.set_account_premium(false, None).await;
    let r = rest_json(&app, &format!("/rest/ping?f=json&apiKey={cle}")).await;
    assert_eq!(code_d_erreur(&r), 50, "{r}");

    // Éteindre reste possible sans Premium.
    let (code, corps) = json_de(
        &app,
        "PUT",
        "/api/v1/subsonic/reglage",
        json!({"enabled": false}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{corps}");
}

#[tokio::test]
async fn cle_valide_ping_repond_en_xml_et_en_json_7855() {
    let (_state, app, cle, _) = banc_ouvert().await;

    let r = rest_json(
        &app,
        &format!("/rest/ping.view?f=json&apiKey={cle}&c=test&v=1.16.1"),
    )
    .await;
    assert_eq!(r["status"], "ok", "{r}");
    assert_eq!(r["openSubsonic"], true);
    assert_eq!(r["type"], "tune");

    let (s, type_, xml) = brut(&app, "GET", &format!("/rest/ping?apiKey={cle}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(type_.starts_with("text/xml"), "type {type_}");
    assert!(xml.starts_with("<?xml"), "{xml}");
    assert!(
        xml.contains(r#"<subsonic-response "#) && xml.contains(r#"status="ok""#),
        "{xml}"
    );
    assert!(
        xml.contains(r#"xmlns="http://subsonic.org/restapi""#),
        "{xml}"
    );

    // POST aussi (les clients qui envoient la requête par formulaire).
    let (s, _, xml) = brut(&app, "POST", &format!("/rest/ping?apiKey={cle}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(xml.contains(r#"status="ok""#), "{xml}");
}

#[tokio::test]
async fn cle_inconnue_ou_absente_est_refusee_7855() {
    let (_state, app, _cle, _) = banc_ouvert().await;
    let r = rest_json(&app, "/rest/ping?f=json&apiKey=tune_sk_0000").await;
    assert_eq!(code_d_erreur(&r), 44, "clé inconnue : {r}");
    let r = rest_json(&app, "/rest/ping?f=json").await;
    assert_eq!(code_d_erreur(&r), 10, "clé absente : {r}");
    let r = rest_json(&app, "/rest/getUser?f=json").await;
    assert_eq!(code_d_erreur(&r), 10, "aucune méthode sans clé : {r}");

    let (_, _, xml) = brut(&app, "GET", "/rest/ping?apiKey=faux", None).await;
    assert!(
        xml.contains(r#"status="failed""#) && xml.contains(r#"code="44""#),
        "{xml}"
    );
}

#[tokio::test]
async fn cle_revoquee_est_refusee_7855() {
    let (_state, app, cle, id) = banc_ouvert().await;
    let r = rest_json(&app, &format!("/rest/ping?f=json&apiKey={cle}")).await;
    assert_eq!(r["status"], "ok");

    let (s, _, _) = brut(&app, "DELETE", &format!("/api/v1/subsonic/cles/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    let r = rest_json(&app, &format!("/rest/ping?f=json&apiKey={cle}")).await;
    assert_eq!(
        code_d_erreur(&r),
        44,
        "une clé révoquée doit être refusée : {r}"
    );

    let (s, _, _) = brut(&app, "DELETE", "/api/v1/subsonic/cles/inexistante", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn jeton_md5_et_mot_de_passe_sont_refuses_7855() {
    let (_state, app, cle, _) = banc_ouvert().await;
    let r = rest_json(
        &app,
        "/rest/ping?f=json&u=default&t=26719a1196d2a940705a59634eb18eab&s=c19b2d",
    )
    .await;
    assert_eq!(code_d_erreur(&r), 42, "jeton md5 : {r}");
    let r = rest_json(&app, "/rest/ping?f=json&u=default&p=secret").await;
    assert_eq!(code_d_erreur(&r), 42, "mot de passe : {r}");
    let r = rest_json(&app, &format!("/rest/ping?f=json&u=default&apiKey={cle}")).await;
    assert_eq!(code_d_erreur(&r), 43, "apiKey avec u : {r}");
    let r = rest_json(&app, &format!("/rest/ping?f=json&t=x&s=y&apiKey={cle}")).await;
    assert_eq!(code_d_erreur(&r), 43, "apiKey avec jeton : {r}");
}

#[tokio::test]
async fn la_cle_n_est_jamais_relisible_apres_creation_7855() {
    let (state, app, cle, id) = banc_ouvert().await;
    let (code, liste) = get_json(&app, "/api/v1/subsonic/cles").await;
    assert_eq!(code, StatusCode::OK);
    let texte = liste.to_string();
    assert!(texte.contains(&id), "la clé doit être listée : {texte}");
    assert!(texte.contains("Symfonium"));
    assert!(
        !texte.contains(&cle),
        "la liste ne doit jamais rendre la clé : {texte}"
    );

    let lignes = state
        .backend
        .query_many("SELECT * FROM cles_clients", &[])
        .unwrap();
    for ligne in &lignes {
        for valeur in ligne {
            if let Some(t) = valeur.as_str() {
                assert!(
                    !t.contains(&cle),
                    "la base ne doit jamais garder la clé en clair : {t}"
                );
            }
        }
    }
}

#[tokio::test]
async fn methodes_du_socle_en_json_7855() {
    let (_state, app, cle, _) = banc_ouvert().await;

    let r = rest_json(&app, &format!("/rest/getLicense?f=json&apiKey={cle}")).await;
    assert_eq!(r["license"]["valid"], true, "{r}");

    let r = rest_json(
        &app,
        &format!("/rest/getOpenSubsonicExtensions.view?f=json&apiKey={cle}"),
    )
    .await;
    let noms: Vec<&str> = r["openSubsonicExtensions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(noms.contains(&"apiKeyAuthentication"), "{r}");

    let r = rest_json(&app, &format!("/rest/getMusicFolders?f=json&apiKey={cle}")).await;
    assert_eq!(r["musicFolders"]["musicFolder"][0]["id"], 1, "{r}");

    let r = rest_json(&app, &format!("/rest/getUser?f=json&apiKey={cle}")).await;
    assert_eq!(r["user"]["username"], "default", "{r}");
    assert_eq!(r["user"]["streamRole"], true);

    let r = rest_json(
        &app,
        &format!("/rest/getUser?f=json&apiKey={cle}&username=personne"),
    )
    .await;
    assert_eq!(code_d_erreur(&r), 70, "{r}");

    let r = rest_json(&app, &format!("/rest/methodeInconnue?f=json&apiKey={cle}")).await;
    assert_eq!(code_d_erreur(&r), 70, "{r}");
}

#[tokio::test]
async fn methodes_du_socle_en_xml_7855() {
    let (_state, app, cle, _) = banc_ouvert().await;
    let (_, _, xml) = brut(
        &app,
        "GET",
        &format!("/rest/getMusicFolders?apiKey={cle}"),
        None,
    )
    .await;
    assert!(
        xml.contains(r#"<musicFolders><musicFolder id="1" name="Tune"/></musicFolders>"#),
        "{xml}"
    );
    let (_, _, xml) = brut(
        &app,
        "GET",
        &format!("/rest/getOpenSubsonicExtensions?apiKey={cle}"),
        None,
    )
    .await;
    assert!(
        xml.contains(r#"<openSubsonicExtensions name="apiKeyAuthentication"><versions>1</versions></openSubsonicExtensions>"#),
        "{xml}"
    );
}

#[tokio::test]
async fn le_reglage_ne_s_allume_pas_par_l_ecriture_generique_7855() {
    let (state, app) = banc();
    state.license.set_account_premium(true, None).await;
    let _ = json_de(
        &app,
        "PATCH",
        "/api/v1/system/config",
        json!({"subsonic_enabled": "true"}),
    )
    .await;
    let (_, corps) = get_json(&app, "/api/v1/subsonic/reglage").await;
    assert_eq!(
        corps["enabled"], false,
        "PATCH /system/config ne doit pas allumer l'accès Subsonic"
    );
    let (s, _, _) = brut(&app, "GET", "/rest/ping", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
