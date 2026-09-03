//! Le crossfeed dit quand il n'agit pas — #2742.
//!
//! Tades (HIFIMAN Serenade, Tune 0.9.119 · Windows) : « Crossfeed n'a aucune
//! action ». Le serveur avait raison sur le fond — le crossfeed est un effet de
//! CASQUE, appliqué par la sortie LOCALE et par elle seule — mais il l'imposait
//! en silence.
//!
//! Les TROIS sites qui installent un `CrossfeedProcessor` sont dans
//! `orchestrator.rs`, tous derrière la même double garde
//! `device_id.starts_with("local:")` + `downcast_ref::<LocalOutput>()` :
//! le chemin de lecture, `refresh_zone_crossfeed` et `refresh_zone_pure_dsp`.
//! Le chemin réseau, lui, passe par `transcode_source_to_file`, dont la
//! signature ne porte que l'égaliseur, le convolveur et le ReplayGain — jamais
//! de crossfeed. Sur une zone DLNA le réglage n'a donc **aucun chemin de code**.
//!
//! Et pourtant `GET /zones/{id}/dsp` le rendait pour n'importe quelle zone et
//! `PUT` le persistait pour n'importe quelle zone, sans un mot. Le seul indice
//! était `crossfeed_applied_live: false`, que son propre commentaire déclare
//! ambigu : faux signifie aussi bien « rien ne joue » que « jamais ».
//!
//! Même défaut que #3192 (« mode exclusif » décoché sans effet sous ASIO), donc
//! même vocabulaire : `reason` = code stable, `detail` = phrase en clair.
//!
//! Tout passe par `tune_server::routes::router(state)` — le routeur réel, son
//! préfixe `/api/v1` et les chemins exacts que tape le client. Aucune
//! transcription de la règle.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

/// Un serveur en mémoire **Premium** : `PUT /zones/{id}/dsp` est derrière la
/// garde `Feature::DspEq` (#2419), et ce fichier ne mesure pas la licence.
async fn app_avec_zones() -> (axum::Router, i64, i64) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    // Le préfixe `local:` n'est pas décoratif : c'est LUI, et lui seul, que
    // l'orchestrateur interroge pour distinguer une carte son d'un renderer
    // réseau (cf. le commentaire de `create_zone`).
    let locale = repo
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let reseau = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-marantz"))
        .unwrap();
    (tune_server::routes::router(state), locale, reseau)
}

/// Le même, en gardant l'état sous la main pour armer le mode PURE.
async fn app_pure() -> (axum::Router, i64) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Casque PURE", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(&format!("zone_{zone}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    (tune_server::routes::router(state), zone)
}

async fn lire_dsp(app: &axum::Router, zone: i64) -> (StatusCode, Value) {
    reponse(
        app,
        Request::get(format!("/api/v1/zones/{zone}/dsp"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn ecrire_crossfeed(app: &axum::Router, zone: i64, enabled: bool) -> (StatusCode, Value) {
    let corps = json!({
        "crossfeed": { "enabled": enabled, "amount": 0.35, "delay_ms": 0.4 }
    });
    reponse(
        app,
        Request::put(format!("/api/v1/zones/{zone}/dsp"))
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
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

// ---------------------------------------------------------------------------
// 1. LE TÉMOIN — une zone à sortie locale ne bouge pas.
// ---------------------------------------------------------------------------

/// Sur `local:…`, le crossfeed s'applique et **rien** ne vient s'ajouter à
/// l'écran : `unavailable` faux, aucun motif, aucune explication.
///
/// C'est le témoin de tout le ticket. Si ce test rougit, la correction du cas
/// réseau a désarmé le cas nominal — exactement ce que Tades cherchait à
/// obtenir.
#[tokio::test]
async fn le_temoin_une_zone_locale_applique_le_crossfeed_sans_rien_annoncer() {
    let (app, locale, _) = app_avec_zones().await;

    let (status, corps) = ecrire_crossfeed(&app, locale, true).await;
    assert_eq!(status, StatusCode::OK);
    let st = &corps["crossfeed_status"];
    assert_eq!(
        st["effective"].as_bool(),
        Some(true),
        "sortie locale hors PURE : le crossfeed s'applique — {corps}"
    );
    assert_eq!(st["unavailable"].as_bool(), Some(false));
    assert!(st["reason"].is_null(), "aucun motif à annoncer : {st}");
    assert!(st["detail"].is_null());

    // Et la relecture dit la même chose.
    let (status, corps) = lire_dsp(&app, locale).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["crossfeed"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        corps["crossfeed_status"]["effective"].as_bool(),
        Some(true),
        "{corps}"
    );
    assert_eq!(
        corps["crossfeed_status"]["unavailable"].as_bool(),
        Some(false)
    );
}

// ---------------------------------------------------------------------------
// 2. LE DÉFAUT — une zone réseau le DIT, au lieu de se taire.
// ---------------------------------------------------------------------------

/// Le cœur de #2742 : sur une zone DLNA, activer le crossfeed rend un statut
/// qui dit `unavailable`, avec un motif STABLE et une phrase en clair.
///
/// Avant, cette réponse ne portait que `crossfeed` (la valeur enregistrée) et
/// `crossfeed_applied_live: false` — dont le commentaire dit lui-même qu'il ne
/// signale pas un échec. L'utilisateur ne pouvait rien en déduire.
#[tokio::test]
async fn une_zone_reseau_annonce_que_le_crossfeed_n_agira_pas() {
    let (app, _, reseau) = app_avec_zones().await;

    let (status, corps) = ecrire_crossfeed(&app, reseau, true).await;
    assert_eq!(status, StatusCode::OK);
    let st = &corps["crossfeed_status"];
    assert!(
        !st.is_null(),
        "la réponse au clic doit porter le statut : {corps}"
    );
    assert_eq!(
        st["effective"].as_bool(),
        Some(false),
        "aucun des trois sites d'installation n'est atteignable ici — {corps}"
    );
    assert_eq!(
        st["unavailable"].as_bool(),
        Some(true),
        "le contrôle doit être annoncé INDISPONIBLE, pas simplement inactif"
    );
    assert_eq!(
        st["reason"].as_str(),
        Some("non_local_output"),
        "le client lit ce code pour choisir sa traduction : {st}"
    );
    let detail = st["detail"].as_str().unwrap_or_default();
    assert!(
        !detail.is_empty(),
        "une contrainte sans explication, c'est le défaut de #2742"
    );
    assert!(
        detail.contains("locale"),
        "l'explication doit dire ce que l'utilisateur PEUT faire : {detail}"
    );
    assert_eq!(
        st["requested"].as_bool(),
        Some(true),
        "`requested` garde ce que l'utilisateur a demandé, sinon l'écran ne \
         peut pas dire que son choix est resté lettre morte"
    );
}

/// `GET` le dit aussi — et **même quand la case est décochée**.
///
/// La question n'est pas « le réglage a-t-il été changé ? » mais « ce réglage
/// a-t-il encore un sens sur cette zone ? ». Sans cela le client ne verrouille
/// le contrôle qu'APRÈS que l'utilisateur a cliqué pour rien.
#[tokio::test]
async fn la_relecture_verrouille_le_controle_meme_case_decochee() {
    let (app, _, reseau) = app_avec_zones().await;

    // Aucune écriture : le réglage n'a jamais été touché sur cette zone.
    let (status, corps) = lire_dsp(&app, reseau).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["crossfeed"]["enabled"].as_bool(),
        Some(false),
        "défaut inchangé"
    );
    let st = &corps["crossfeed_status"];
    assert_eq!(st["requested"].as_bool(), Some(false));
    assert_eq!(
        st["unavailable"].as_bool(),
        Some(true),
        "le contrôle doit être verrouillé AVANT le premier clic : {corps}"
    );
    assert_eq!(st["reason"].as_str(), Some("non_local_output"));
}

/// Le mode PURE, sur une sortie locale, désarme le crossfeed — et le dit avec
/// son propre motif. `load_crossfeed_processor` rendait déjà `None` en PURE ;
/// ce qui manquait était de l'annoncer.
#[tokio::test]
async fn le_mode_pure_annonce_qu_il_desarme_le_crossfeed() {
    let (app, zone) = app_pure().await;

    let (status, corps) = ecrire_crossfeed(&app, zone, true).await;
    assert_eq!(status, StatusCode::OK);
    let st = &corps["crossfeed_status"];
    assert_eq!(st["effective"].as_bool(), Some(false));
    assert_eq!(st["unavailable"].as_bool(), Some(true));
    assert_eq!(st["reason"].as_str(), Some("pure_mode"), "{corps}");
}

// ---------------------------------------------------------------------------
// 3. LA CONTRE-ÉPREUVE — dire la vérité n'a rien retiré.
// ---------------------------------------------------------------------------

/// Le réglage reste PERSISTÉ et RELU sur une zone réseau.
///
/// La correction honnête est d'annoncer l'indisponibilité, PAS de refuser
/// l'écriture : une zone peut changer de sortie, et #1786 a déjà payé le prix
/// d'un crossfeed qui ne se rappelait pas de lui-même. Ce test interdit la
/// « correction » qui consisterait à jeter la valeur.
#[tokio::test]
async fn le_reglage_reste_persiste_sur_une_zone_reseau() {
    let (app, _, reseau) = app_avec_zones().await;

    let (_, ecrit) = ecrire_crossfeed(&app, reseau, true).await;
    assert_eq!(ecrit["crossfeed"]["enabled"].as_bool(), Some(true));
    assert_eq!(ecrit["crossfeed"]["amount"].as_f64(), Some(0.35));

    let (_, relu) = lire_dsp(&app, reseau).await;
    assert_eq!(
        relu["crossfeed"]["enabled"].as_bool(),
        Some(true),
        "la valeur doit survivre : la zone peut changer de sortie — {relu}"
    );
    assert_eq!(relu["crossfeed"]["amount"].as_f64(), Some(0.35));
    assert_eq!(relu["crossfeed"]["delay_ms"].as_f64(), Some(0.4));
}

/// Le contrat additif : les champs d'avant sont TOUS encore là, à l'identique.
/// Un client qui ne lit pas `crossfeed_status` voit le même écran qu'avant.
#[tokio::test]
async fn les_champs_historiques_de_la_route_dsp_sont_intacts() {
    let (app, locale, _) = app_avec_zones().await;

    let (_, corps) = lire_dsp(&app, locale).await;
    for champ in ["zone_id", "eq_profile", "crossfeed"] {
        assert!(
            corps.get(champ).is_some(),
            "champ historique disparu de GET /dsp : {champ} — {corps}"
        );
    }
    for champ in ["enabled", "amount", "delay_ms"] {
        assert!(
            corps["crossfeed"].get(champ).is_some(),
            "champ historique disparu de `crossfeed` : {champ}"
        );
    }

    let (_, ecrit) = ecrire_crossfeed(&app, locale, true).await;
    for champ in [
        "zone_id",
        "crossfeed",
        "crossfeed_applied_live",
        "eq_applied_live",
    ] {
        assert!(
            ecrit.get(champ).is_some(),
            "champ historique disparu de PUT /dsp : {champ} — {ecrit}"
        );
    }
}

/// Un corps sans `crossfeed` ne fabrique pas de statut : `null`, pas un objet
/// inventé. Rien n'a été demandé, il n'y a rien à répondre.
#[tokio::test]
async fn sans_demande_de_crossfeed_la_reponse_ne_publie_aucun_statut() {
    let (app, _, reseau) = app_avec_zones().await;

    let (status, corps) = reponse(
        &app,
        Request::put(format!("/api/v1/zones/{reseau}/dsp"))
            .header("Content-Type", "application/json")
            .body(Body::from(json!({ "dsp_enabled": false }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        corps["crossfeed_status"].is_null(),
        "aucun crossfeed demandé : aucun statut à rendre — {corps}"
    );
}
