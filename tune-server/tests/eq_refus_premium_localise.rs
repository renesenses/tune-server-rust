//! L'égaliseur sans licence : ce qui reste LISIBLE, et ce qui doit REFUSER — #2419.
//!
//! Le ticket dit « EQ cliquable sans licence, le 402 est avalé ». Deux moitiés,
//! et elles ne se corrigent pas dans le même sens :
//!
//! 1. **La lecture reste gratuite.** `GET /zones/{id}/eq` n'a pas de garde, et
//!    ne doit pas en recevoir. `EqualizerView.svelte` l'appelle dans son
//!    `onMount` SANS condition de licence, pour dessiner la courbe réelle
//!    derrière le bandeau `premium-gate` qu'il affiche quand `!$isPremium`. La
//!    garder ferait deux dégâts d'un coup : l'écran perdrait son contenu, et la
//!    fenêtre premium de `fetchJSON` s'ouvrirait à la simple OUVERTURE de
//!    l'égaliseur. C'est aussi la règle uniforme du domaine — `get_zone_dsp`,
//!    `eq_status`, `list_presets`, `get_bands` lisent sans droit.
//!    Les tests de ce fichier VERROUILLENT cette gratuité : quelqu'un qui
//!    « corrigerait » #2419 en gardant la lecture les fait rougir.
//!
//! 2. **L'écriture refuse, et son refus se lit.** Le 402 existait déjà, mais sa
//!    phrase était composée en anglais (`"… requires Tune Premium"`) et le
//!    client web l'affiche telle quelle (`api.ts` : `notifications.error(
//!    body?.message)`). Dans une interface traduite en dix langues. Le refus
//!    porte désormais un `code` stable ET une phrase qui suit
//!    l'`Accept-Language`.
//!
//! Et le témoin, sans lequel tout le reste ne prouve rien : **une licence
//! valide ne voit RIEN changer**, ni en lecture ni en écriture.
//!
//! Tout passe par `tune_server::routes::router(state)` — le routeur réel, son
//! préfixe `/api/v1` et son repli 404 — et par les chemins exacts que tape le
//! client. Aucune transcription de la logique de garde.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

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
    tune_server::routes::router(state)
}

/// Le même, **Premium** : le témoin.
async fn app_premium() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    tune_server::routes::router(state)
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

/// Sans licence, `POST /zones/{id}/eq` répond **402** — et le refus est nommé.
#[tokio::test]
async fn l_ecriture_de_l_egaliseur_refuse_sans_licence_et_se_nomme() {
    let app = app_gratuit().await;
    let (status, corps) = ecrire(&app, EQ, bandes(), Some("fr")).await;

    assert_eq!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "écrire l'égaliseur sans droit doit refuser : {corps}"
    );
    assert_eq!(corps["error"], "premium_required");
    assert_eq!(
        corps["code"], "dsp_eq",
        "le refus doit porter le CODE stable que le client traduit : {corps}"
    );
    assert_eq!(corps["upgrade_url"], "https://mozaiklabs.fr/pricing");
}

/// Le refus n'a **rien écrit** : la relecture gratuite rend toujours la courbe
/// plate. Un 402 qui persisterait quand même serait le pire des deux mondes.
#[tokio::test]
async fn le_refus_d_ecriture_ne_persiste_aucune_bande() {
    let app = app_gratuit().await;
    let (status, _) = ecrire(&app, EQ, bandes(), Some("fr")).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);

    let (status, corps) = lire(&app, EQ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["bands"].as_array().map(|b| b.len()),
        Some(0),
        "un refus ne doit rien laisser derrière lui : {corps}"
    );
    assert_eq!(corps["enabled"], false, "corps : {corps}");
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

    let (_, fr) = ecrire(&app, EQ, bandes(), Some("fr-FR,fr;q=0.9")).await;
    let (_, de) = ecrire(&app, EQ, bandes(), Some("de")).await;
    let (_, en) = ecrire(&app, EQ, bandes(), Some("en-US,en;q=0.8")).await;
    let (_, ja) = ecrire(&app, EQ, bandes(), Some("ja")).await;

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
            phrase(v).contains("DSP & EQ"),
            "le refus doit nommer le droit manquant : {v}"
        );
        assert_eq!(v["code"], "dsp_eq", "le code ne se traduit PAS : {v}");
    }
}

/// Une locale que l'interface ne parle pas retombe sur le français, le défaut
/// de l'application — jamais sur la clé brute ni sur du vide.
#[tokio::test]
async fn une_locale_inconnue_retombe_sur_le_francais() {
    let app = app_gratuit().await;
    let (_, corps) = ecrire(&app, EQ, bandes(), Some("kl-GL")).await;
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
    let (status, corps) = ecrire(&app, EQ, bandes(), None).await;

    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    let phrase = corps["message"].as_str().unwrap_or_default();
    assert!(!phrase.is_empty(), "message vide : {corps}");
    assert!(
        !phrase.contains("premium.required"),
        "clé de traduction non résolue : {corps}"
    );
    assert_eq!(corps["code"], "dsp_eq");
}

/// `PUT /zones/{id}/dsp` est l'autre moitié du même écran : son refus se lit
/// dans la même langue, avec le même code. Deux routes, un seul contrat.
#[tokio::test]
async fn le_refus_du_dsp_de_zone_se_lit_comme_celui_de_l_egaliseur() {
    let app = app_gratuit().await;
    let (status, corps) = ecrire_put(&app, DSP, json!({ "dsp_enabled": true }), Some("de")).await;

    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "corps : {corps}");
    assert_eq!(corps["error"], "premium_required");
    assert_eq!(corps["code"], "dsp_eq");
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
