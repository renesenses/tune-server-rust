//! EQ is free (#4363). Crossfeed keeps its separate Premium entitlement.
//! Preserve #2419's localized refusal witnesses on the paid operation;
//! exercise real routes, persisted settings, mixed-request atomicity and presets.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ZONE: i64 = 1;
const EQ: &str = "/api/v1/zones/1/eq";
const DSP: &str = "/api/v1/zones/1/dsp";

/// Un serveur en mémoire **sans licence** — le palier Free, celui du ticket.
async fn app_gratuit() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    installer_l_egaliseur(&state);
    tune_server::routes::router(state)
}

/// Le même, **Premium** : le témoin.
async fn app_premium() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    installer_l_egaliseur(&state);
    tune_server::routes::router(state)
}

/// L'égaliseur est un greffon facultatif (v0.9.156) : Free ou Premium, il faut
/// l'avoir installé depuis le catalogue — la clé que pose
/// `POST /plugins/equalizer/install`.
fn installer_l_egaliseur(state: &tune_server::state::AppState) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_equalizer_installed", "true")
        .unwrap();
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    reponse(app, Request::get(chemin).body(Body::empty()).unwrap()).await
}

/// `langue` = la valeur d'`Accept-Language`, ou `None` pour n'en envoyer aucune.
async fn ecrire(
    app: &axum::Router,
    chemin: &str,
    corps: Value,
    langue: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::post(chemin).header("Content-Type", "application/json");
    if let Some(l) = langue {
        req = req.header("Accept-Language", l);
    }
    reponse(app, req.body(Body::from(corps.to_string())).unwrap()).await
}

async fn ecrire_put(
    app: &axum::Router,
    chemin: &str,
    corps: Value,
    langue: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::put(chemin).header("Content-Type", "application/json");
    if let Some(l) = langue {
        req = req.header("Accept-Language", l);
    }
    reponse(app, req.body(Body::from(corps.to_string())).unwrap()).await
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Une courbe quelconque, mais VALIDE : le refus doit venir de la licence, pas
/// d'un corps mal formé qui rendrait 400 et masquerait ce qu'on mesure.
fn bandes() -> Value {
    json!({
        "enabled": true,
        "bands": [{ "freq": 1000.0, "gain": 3.0, "q": 1.41, "type": "peak" }]
    })
}

// ---------------------------------------------------------------------------
// 1. La lecture reste GRATUITE — le verrou anti-« correction » de #2419.
// ---------------------------------------------------------------------------

/// Sans aucune licence, `GET /zones/{id}/eq` répond **200** et une courbe.
///
/// Poser `require_premium` sur `get_eq` fait rougir ce test — et c'est
/// exactement ce qu'on veut interdire : l'écran Égaliseur du palier Free se
/// dessine à partir de cette réponse.
#[tokio::test]
async fn la_lecture_de_l_egaliseur_reste_ouverte_sans_licence() {
    let app = app_gratuit().await;
    let (status, corps) = lire(&app, EQ).await;

    assert_ne!(status, StatusCode::NOT_FOUND, "route non montée sur {EQ}");
    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "la LECTURE de l'égaliseur ne doit jamais coûter un droit : {corps}"
    );
    assert_eq!(status, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["zone_id"], ZONE);
    assert!(
        corps.get("bands").is_some_and(|b| b.is_array()),
        "l'écran grisé a besoin des bandes pour se dessiner : {corps}"
    );
    assert!(
        corps.get("error").is_none(),
        "une lecture gratuite ne rend pas un refus : {corps}"
    );
}

/// La lecture du DSP de zone reste ouverte elle aussi : même écran, même
/// `onMount`, `api.getDsp(zoneId)` juste après `api.getEq(zoneId)`.
#[tokio::test]
async fn la_lecture_du_dsp_de_zone_reste_ouverte_sans_licence() {
    let app = app_gratuit().await;
    let (status, corps) = lire(&app, DSP).await;

    assert_ne!(status, StatusCode::NOT_FOUND, "route non montée sur {DSP}");
    assert_eq!(status, StatusCode::OK, "corps : {corps}");
    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "garder cette lecture viderait l'écran Égaliseur du palier Free : {corps}"
    );
}

// ---------------------------------------------------------------------------
// 2. L'écriture REFUSE, et son refus est lisible.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn premium_sdk_free_equalizer_writes_and_reads_real_bands() {
    let app = app_gratuit().await;
    let (status, value) = ecrire(&app, EQ, bandes(), Some("fr")).await;
    assert_eq!(status, StatusCode::OK, "FREE EQ refused: {value}");
    let (_, stored) = lire(&app, EQ).await;
    assert_eq!(stored["enabled"], true);
    assert_eq!(stored["bands"][0]["gain"], 3.0);
    let (status, value) = ecrire(
        &app,
        "/api/v1/eq/presets",
        json!({"name":"Free preset","bands":bandes()["bands"]}),
        Some("fr"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "FREE preset refused: {value}");
    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(config["premium_features"]["dsp_eq"], true, "{config}");
    assert_eq!(config["premium_features"]["crossfeed"], false, "{config}");
}

#[tokio::test]
async fn premium_sdk_mixed_free_request_refuses_crossfeed_without_writing_eq() {
    let app = app_gratuit().await;
    let profile = tune_core::audio::eq::EqProfile {
        enabled: true,
        bass_gain_db: 4.0,
        ..Default::default()
    };
    let (status, value) = ecrire_put(
        &app,
        DSP,
        json!({"eq_profile": profile, "crossfeed":{"enabled":true}}),
        Some("fr"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{value}");
    assert_eq!(value["code"], "crossfeed");
    let (_, stored) = lire(&app, EQ).await;
    assert_eq!(
        stored["enabled"], false,
        "mixed request partially persisted EQ: {stored}"
    );
    let (status, value) = ecrire_put(&app, DSP, json!({"eq_profile": profile}), Some("fr")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "EQ-only DSP request refused: {value}"
    );
    let (_, stored) = lire(&app, DSP).await;
    assert_eq!(stored["eq_profile"]["bass_gain_db"], 4.0, "{stored}");
}

// ---------------------------------------------------------------------------
// 3. Le refus parle la langue de l'utilisateur — le cœur de #2419.
// ---------------------------------------------------------------------------

/// La phrase du refus suit `Accept-Language`, et n'est plus figée en anglais.
///
/// Le client web affiche `body.message` TEL QUEL (`api.ts`,
/// `notifications.error(body?.message || …)`). Tant que le serveur composait
/// « DSP & EQ requires Tune Premium », une interface en français, en allemand
/// ou en japonais affichait cette phrase anglaise.
#[tokio::test]
async fn la_phrase_du_refus_suit_l_entete_accept_language() {
    let app = app_gratuit().await;

    let (_, fr) = ecrire_put(
        &app,
        DSP,
        json!({"crossfeed":{"enabled":true}}),
        Some("fr-FR,fr;q=0.9"),
    )
    .await;
    let (_, de) = ecrire_put(&app, DSP, json!({"crossfeed":{"enabled":true}}), Some("de")).await;
    let (_, en) = ecrire_put(
        &app,
        DSP,
        json!({"crossfeed":{"enabled":true}}),
        Some("en-US,en;q=0.8"),
    )
    .await;
    let (_, ja) = ecrire_put(&app, DSP, json!({"crossfeed":{"enabled":true}}), Some("ja")).await;

    let phrase = |v: &Value| v["message"].as_str().unwrap_or_default().to_string();

    assert!(
        phrase(&fr).contains("nécessite Tune Premium"),
        "refus en français attendu : {fr}"
    );
    assert!(
        phrase(&de).contains("erfordert Tune Premium"),
        "refus en allemand attendu : {de}"
    );
    assert!(
        phrase(&en).contains("requires Tune Premium"),
        "refus en anglais attendu : {en}"
    );
    assert!(
        phrase(&ja).contains("Tune Premium が必要です"),
        "refus en japonais attendu : {ja}"
    );

    // Le défaut mesuré : quatre langues demandées, une seule phrase rendue.
    assert_ne!(phrase(&fr), phrase(&de), "fr et de rendent la même phrase");
    assert_ne!(phrase(&fr), phrase(&en), "fr et en rendent la même phrase");
    assert_ne!(phrase(&fr), phrase(&ja), "fr et ja rendent la même phrase");

    // Le nom du droit reste un nom de produit, il traverse les traductions.
    for v in [&fr, &de, &en, &ja] {
        assert!(
            phrase(v).contains("Crossfeed"),
            "le refus doit nommer le droit manquant : {v}"
        );
        assert_eq!(v["code"], "crossfeed", "le code ne se traduit PAS : {v}");
    }
}

/// Une locale que l'interface ne parle pas retombe sur le français, le défaut
/// de l'application — jamais sur la clé brute ni sur du vide.
#[tokio::test]
async fn une_locale_inconnue_retombe_sur_le_francais() {
    let app = app_gratuit().await;
    let (_, corps) = ecrire_put(
        &app,
        DSP,
        json!({"crossfeed":{"enabled":true}}),
        Some("kl-GL"),
    )
    .await;
    let phrase = corps["message"].as_str().unwrap_or_default();

    assert!(
        phrase.contains("nécessite Tune Premium"),
        "repli français attendu : {corps}"
    );
    assert!(
        !phrase.contains("premium.required"),
        "la clé de traduction ne doit jamais fuir dans la réponse : {corps}"
    );
}

/// Sans `Accept-Language` du tout — un client tiers, `curl` — le refus reste
/// une phrase, pas une clé.
#[tokio::test]
async fn un_refus_sans_entete_de_langue_reste_une_phrase() {
    let app = app_gratuit().await;
    let (status, corps) = ecrire_put(&app, DSP, json!({"crossfeed":{"enabled":true}}), None).await;

    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    let phrase = corps["message"].as_str().unwrap_or_default();
    assert!(!phrase.is_empty(), "message vide : {corps}");
    assert!(
        !phrase.contains("premium.required"),
        "clé de traduction non résolue : {corps}"
    );
    assert_eq!(corps["code"], "crossfeed");
}

/// `PUT /zones/{id}/dsp` est l'autre moitié du même écran : son refus se lit
/// dans la même langue, avec le même code. Deux routes, un seul contrat.
#[tokio::test]
async fn le_refus_du_dsp_de_zone_se_lit_comme_celui_de_l_egaliseur() {
    let app = app_gratuit().await;
    let (status, corps) =
        ecrire_put(&app, DSP, json!({"crossfeed":{"enabled":true}}), Some("de")).await;

    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "corps : {corps}");
    assert_eq!(corps["error"], "premium_required");
    assert_eq!(corps["code"], "crossfeed");
    assert!(
        corps["message"]
            .as_str()
            .is_some_and(|m| m.contains("erfordert Tune Premium")),
        "le refus du DSP doit suivre la langue lui aussi : {corps}"
    );
}

// ---------------------------------------------------------------------------
// 4. LE TÉMOIN : une licence valide ne voit rien changer.
// ---------------------------------------------------------------------------

/// Avec Premium, la lecture ET l'écriture se comportent comme avant : 200 des
/// deux côtés, et la courbe écrite se relit. Aucune garde ajoutée n'a débordé.
#[tokio::test]
async fn avec_une_licence_valide_rien_ne_change_ni_en_lecture_ni_en_ecriture() {
    let app = app_premium().await;

    // Lecture : ouverte, comme au palier Free.
    let (status, avant) = lire(&app, EQ).await;
    assert_eq!(status, StatusCode::OK, "corps : {avant}");
    assert_eq!(avant["zone_id"], ZONE);

    // Écriture : acceptée, et AUCUN champ de refus ne s'invite dans la réponse.
    let (status, ecrit) = ecrire(&app, EQ, bandes(), Some("fr")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "une licence valide ne doit jamais voir un refus : {ecrit}"
    );
    assert!(
        ecrit.get("error").is_none() && ecrit.get("code").is_none(),
        "la réponse d'un client licencié ne porte aucun refus : {ecrit}"
    );

    // Et elle a bien atteint le profil que lit l'orchestrateur.
    let (status, apres) = lire(&app, EQ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(apres["enabled"], true, "corps : {apres}");
    let bandes_relues = apres["bands"].as_array().expect("bandes");
    assert_eq!(bandes_relues.len(), 1, "corps : {apres}");
    assert_eq!(bandes_relues[0]["freq"], 1000.0);
    assert_eq!(bandes_relues[0]["gain"], 3.0);
}

/// Le témoin pour l'autre route de l'écran : `PUT /zones/{id}/dsp` accepte
/// toujours avec une licence, quel que soit l'`Accept-Language` envoyé.
#[tokio::test]
async fn avec_une_licence_valide_le_dsp_de_zone_accepte_toujours() {
    let app = app_premium().await;
    let (status, corps) = ecrire_put(&app, DSP, json!({ "dsp_enabled": true }), Some("de")).await;

    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "l'ajout de l'en-tête ne doit pas transformer un droit valide en refus : {corps}"
    );
    assert_eq!(status, StatusCode::OK, "corps : {corps}");
}
