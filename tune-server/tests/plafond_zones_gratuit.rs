//! Le plafond de zones du palier gratuit : UNE règle, et un refus qui se lit —
//! #3673 (quatre implémentations divergentes) et #3672 (phrase anglaise en dur).
//!
//! Ce que ce fichier garde, et pourquoi :
//!
//! 1. **Le refus est traduit.** Ce qui partait du cœur était une phrase
//!    anglaise composée en dur — « Free tier is limited to 3 active zones.
//!    Upgrade to Tune Premium… » — servie telle quelle par le client web
//!    (`api.ts` : `notifications.error(body?.message)`) dans une interface
//!    traduite. C'est exactement la moitié de #2419 que ce chemin-ci avait
//!    ratée : `require_premium` la compose depuis `i18n_server.json` suivant
//!    l'`Accept-Language`, le plafond de zones non.
//!
//! 2. **Le refus nomme la bonne chose.** Le corps annonçait
//!    `"feature": "Unlimited Zones"` : un prospect (Claudio Osorio, 08/09/2026)
//!    qui venait de cliquer sur son enceinte BluOS y a lu « Premium » et en a
//!    conclu que DLNA, AirPlay 2 et BluOS étaient payants. Aucun protocole ne
//!    l'est. La phrase doit dire le NOMBRE DE ZONES *et* que les protocoles
//!    sont inclus.
//!
//! 3. **Le plafond est le même partout.** `/system/config`, `/cloud/license/
//!    status` et le refus lui-même tenaient chacun leur propre copie du
//!    chiffre. Ils lisent désormais `LicenseManager::plafond_zones`.
//!
//! 4. **L'assiette de #667 tient.** Une zone auto-découverte jamais jouée ne
//!    consomme rien (c'est ce qui a débloqué JeromeQ, forum #783), et une zone
//!    déjà active rejoue sans jamais rencontrer le plafond.
//!
//! Tout passe par `tune_server::routes::router(state)` — le routeur réel, son
//! préfixe `/api/v1` — et par le chemin exact que tape le client web
//! (`POST /zones/{id}/play` avec `{ "track_id": … }`). Aucune transcription de
//! la logique de garde : le test ne sait pas COMMENT le serveur compte, il
//! mesure ce que la route rend.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::zone_repo::ZoneRepo;

/// La phrase anglaise en dur d'avant #3672. Si elle réapparaît dans un
/// `message`, c'est que le refus est reparti du cœur tout composé.
const PHRASE_EN_DUR: &str = "Free tier is limited";

/// Un serveur en mémoire **sans licence** : le palier Free, celui du ticket.
/// Le plafond par défaut de `TuneConfig` est 3 — la valeur que la page
/// tarifaire annonce.
fn etat_gratuit() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Crée `n` zones en ligne. Elles naissent **dormantes** : découvertes sur le
/// réseau, jamais jouées.
fn zones_dormantes(state: &tune_server::state::AppState, n: usize) -> Vec<i64> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    (0..n)
        .map(|i| {
            let id = repo
                .create(
                    &format!("Enceinte {i}"),
                    Some("dlna"),
                    Some(&format!("uuid:enceinte-{i}")),
                )
                .unwrap();
            repo.update_online(id, true).unwrap();
            id
        })
        .collect()
}

/// Marque une zone comme **activée** : elle a joué au moins une piste, donc
/// elle consomme le quota. C'est le geste exact que fait la lecture
/// (`save_playback_position`), pas une écriture inventée pour le test.
fn activer(state: &tune_server::state::AppState, id: i64) {
    ZoneRepo::with_backend(state.backend.clone())
        .save_playback_position(id, 0, Some(1), Some("local"), None)
        .unwrap();
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

/// Le geste du client : « joue cette piste sur cette zone ».
async fn jouer(app: &axum::Router, zone: i64, langue: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::post(format!("/api/v1/zones/{zone}/play"))
        .header("Content-Type", "application/json");
    if let Some(l) = langue {
        req = req.header("Accept-Language", l);
    }
    reponse(
        app,
        req.body(Body::from(json!({ "track_id": 1 }).to_string()))
            .unwrap(),
    )
    .await
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    reponse(app, Request::get(chemin).body(Body::empty()).unwrap()).await
}

/// Trois zones déjà jouées, une quatrième dormante : c'est la situation du
/// ticket. Rend l'application et l'id de la zone qu'on va tenter de jouer.
fn trois_actives_et_une_de_plus() -> (axum::Router, i64) {
    let state = etat_gratuit();
    let ids = zones_dormantes(&state, 4);
    for id in &ids[..3] {
        activer(&state, *id);
    }
    let quatrieme = ids[3];
    (tune_server::routes::router(state), quatrieme)
}

// ---------------------------------------------------------------------------
// 1. Le refus existe, et il porte les nombres
// ---------------------------------------------------------------------------

/// La 4ᵉ zone est refusée en 402, avec le **code stable** distinct de
/// `premium_required` et les deux nombres que l'écran des zones peut afficher
/// (« 3 / 3 ») au lieu d'un refus nu.
#[tokio::test]
async fn la_quatrieme_zone_est_refusee_avec_son_code_et_ses_nombres() {
    let (app, quatrieme) = trois_actives_et_une_de_plus();
    let (status, corps) = jouer(&app, quatrieme, Some("fr")).await;

    assert_eq!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "la 4e zone active doit etre refusee en 402, corps rendu : {corps}"
    );
    assert_eq!(
        corps["code"], "free_zone_cap_reached",
        "le refus doit porter son terme stable, celui sur lequel un client \
         porte sa propre traduction : {corps}"
    );
    assert_eq!(
        corps["zone_limit"], 3,
        "le plafond doit voyager dans le corps : {corps}"
    );
    assert_eq!(
        corps["zones_actives"], 3,
        "le nombre de zones deja consommees doit voyager aussi : {corps}"
    );
    // `error` reste la famille sur laquelle les clients trient depuis #2178.
    assert_eq!(corps["error"], "premium_required");
}

// ---------------------------------------------------------------------------
// 2. La phrase — #3672
// ---------------------------------------------------------------------------

/// Le cœur du ticket : la phrase suit l'`Accept-Language`, et la phrase
/// anglaise codée en dur a disparu des DEUX langues.
#[tokio::test]
async fn le_refus_parle_la_langue_de_la_requete() {
    let (app, quatrieme) = trois_actives_et_une_de_plus();
    let (_, fr) = jouer(&app, quatrieme, Some("fr")).await;
    let (_, en) = jouer(&app, quatrieme, Some("en")).await;
    let (_, de) = jouer(&app, quatrieme, Some("de")).await;

    let mfr = fr["message"].as_str().unwrap_or_default().to_string();
    let men = en["message"].as_str().unwrap_or_default().to_string();
    let mde = de["message"].as_str().unwrap_or_default().to_string();

    assert!(
        !mfr.is_empty() && !men.is_empty() && !mde.is_empty(),
        "chaque langue doit rendre une phrase : fr={mfr:?} en={men:?} de={mde:?}"
    );
    assert_ne!(
        mfr, men,
        "le francais et l'anglais rendent la MEME phrase : le refus n'est pas traduit"
    );
    assert_ne!(mfr, mde, "l'allemand rend la phrase francaise");
    for (langue, message) in [("fr", &mfr), ("en", &men), ("de", &mde)] {
        assert!(
            !message.contains(PHRASE_EN_DUR),
            "en {langue}, le refus sert encore la phrase anglaise codee en dur : {message:?}"
        );
    }
    // Sans en-tête, le défaut de l'application (fr) — pas d'anglais résiduel.
    let (_, nu) = jouer(&app, quatrieme, None).await;
    assert_eq!(
        nu["message"], mfr,
        "sans Accept-Language, le refus doit tomber sur le defaut francais"
    );
}

/// La moitié de la phrase qui aurait évité le ticket : le refus dit qu'il
/// s'agit d'un **nombre de zones** et que les protocoles réseau sont inclus
/// dans le gratuit. Claudio Osorio a lu « Premium » et compris « BluOS est
/// payant » ; aucun protocole ne l'est.
#[tokio::test]
async fn le_refus_dit_que_les_protocoles_sont_inclus() {
    let (app, quatrieme) = trois_actives_et_une_de_plus();
    for langue in tune_server::i18n::SUPPORTED {
        let (status, corps) = jouer(&app, quatrieme, Some(langue)).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        let message = corps["message"].as_str().unwrap_or_default();
        // Le plafond, en chiffres, dans la phrase : sans lui l'utilisateur ne
        // sait pas de quoi on lui parle.
        assert!(
            message.contains('3'),
            "en {langue}, la phrase ne dit pas le nombre de zones : {message:?}"
        );
        for protocole in ["DLNA", "AirPlay 2", "BluOS", "Chromecast", "OpenHome"] {
            assert!(
                message.contains(protocole),
                "en {langue}, la phrase ne rassure pas sur {protocole} — c'est \
                 precisement ce que le prospect a mal compris : {message:?}"
            );
        }
    }
}

/// Une langue non servie retombe sur le défaut, pas sur la clé nue : un
/// utilisateur hongrois (le client web a 11 locales, le serveur 10) doit lire
/// une phrase, pas `zone.freeCapReached`.
#[tokio::test]
async fn une_langue_non_servie_rend_une_phrase_pas_une_cle() {
    let (app, quatrieme) = trois_actives_et_une_de_plus();
    let (_, corps) = jouer(&app, quatrieme, Some("hu")).await;
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("freeCapReached"),
        "la cle i18n nue est sortie telle quelle : {message:?}"
    );
    assert!(!message.is_empty());
}

// ---------------------------------------------------------------------------
// 3. Une seule règle — #3673
// ---------------------------------------------------------------------------

/// Le chiffre du refus, celui de `/system/config` et celui de
/// `/cloud/license/status` sont le MÊME. Trois endroits en tenaient chacun une
/// copie ; que l'un dérive et ceci rougit.
#[tokio::test]
async fn les_trois_chemins_annoncent_le_meme_plafond() {
    let (app, quatrieme) = trois_actives_et_une_de_plus();
    let (_, refus) = jouer(&app, quatrieme, Some("fr")).await;
    let (_, config) = lire(&app, "/api/v1/system/config").await;
    let (_, licence) = lire(&app, "/api/v1/cloud/license/status").await;

    let du_refus = refus["zone_limit"].as_i64();
    assert_eq!(du_refus, Some(3), "corps du refus : {refus}");
    assert_eq!(
        config["zone_limit"].as_i64(),
        du_refus,
        "/system/config annonce un autre plafond que le refus : {config}"
    );
    assert_eq!(
        licence["zone_limit"].as_i64(),
        du_refus,
        "/cloud/license/status annonce un autre plafond que le refus : {licence}"
    );
}

/// Le plafond suit la configuration, il n'est pas gravé : un serveur monté
/// avec `free_max_zones = 1` refuse à la 2ᵉ zone et le DIT, dans le refus
/// comme dans `/system/config`. Un test qui n'attend que « 3 » ne garde rien
/// contre un chiffre recopié à la main quelque part.
#[tokio::test]
async fn le_plafond_suit_la_configuration_partout() {
    let config = tune_server::config::TuneConfig {
        free_max_zones: 1,
        ..Default::default()
    };
    let state = tune_server::state::AppState::new(":memory:", 0, config).unwrap();
    let ids = zones_dormantes(&state, 2);
    activer(&state, ids[0]);
    let app = tune_server::routes::router(state);

    let (status, refus) = jouer(&app, ids[1], Some("fr")).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(refus["zone_limit"], 1, "corps du refus : {refus}");
    assert_eq!(refus["zones_actives"], 1);
    assert!(
        refus["message"].as_str().unwrap_or_default().contains('1'),
        "la phrase doit dire le plafond REEL, pas 3 : {refus}"
    );
    let (_, conf) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        conf["zone_limit"], 1,
        "/system/config a garde son propre chiffre : {conf}"
    );
}

// ---------------------------------------------------------------------------
// 4. L'assiette de #667 — ce que ce chantier ne doit PAS casser
// ---------------------------------------------------------------------------

/// Cinq zones découvertes, aucune jouée : la première lecture passe. C'est le
/// faux « Premium requis » que #667 a corrigé pour JeromeQ (forum #783) — la
/// découverte réseau ne doit jamais remplir le quota.
#[tokio::test]
async fn cinq_zones_dormantes_ne_consomment_rien() {
    let state = etat_gratuit();
    let ids = zones_dormantes(&state, 5);
    let app = tune_server::routes::router(state);
    let (status, corps) = jouer(&app, ids[4], Some("fr")).await;
    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "des zones jamais jouees ont consomme le quota : {corps}"
    );
}

/// Une zone **déjà active** rejoue sans jamais rencontrer le plafond, même
/// quand le quota est plein : le garde porte sur l'ACTIVATION, pas sur chaque
/// lecture. Sans ceci, un utilisateur gratuit à 3 zones ne pourrait plus rien
/// rejouer du tout.
#[tokio::test]
async fn une_zone_deja_active_rejoue_meme_quota_plein() {
    let state = etat_gratuit();
    let ids = zones_dormantes(&state, 3);
    for id in &ids {
        activer(&state, *id);
    }
    let app = tune_server::routes::router(state);
    let (status, corps) = jouer(&app, ids[0], Some("fr")).await;
    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "une zone deja jouee doit pouvoir rejouer : {corps}"
    );
}

/// Le témoin : avec Premium, rien de tout cela ne se déclenche, et
/// `/system/config` annonce « illimité » (`null`), pas un nombre.
#[tokio::test]
async fn premium_ne_voit_aucun_plafond() {
    let state = etat_gratuit();
    let ids = zones_dormantes(&state, 6);
    for id in &ids[..5] {
        activer(&state, *id);
    }
    state.license.set_account_premium(true, None).await;
    let app = tune_server::routes::router(state);

    let (status, corps) = jouer(&app, ids[5], Some("fr")).await;
    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "Premium a rencontre un plafond : {corps}"
    );
    let (_, conf) = lire(&app, "/api/v1/system/config").await;
    assert!(
        conf["zone_limit"].is_null(),
        "Premium doit etre annonce illimite : {conf}"
    );
}
