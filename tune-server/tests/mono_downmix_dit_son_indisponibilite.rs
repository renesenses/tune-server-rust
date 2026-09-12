//! La sortie mono dit quand elle n'agit pas, au lieu de se taire (#3254).
//!
//! ## Le fait
//!
//! `zone_{id}_mono_downmix` (#2362) est **accepté** par `PATCH /zones/{id}`
//! (`zones.rs`, bloc « Sortie mono ») et **relu** par `GET /zones/{id}` et
//! `GET /zones` (`inject_device_identity`) pour **n'importe quelle** zone —
//! aucune de ces deux surfaces ne regarde le type de sortie.
//!
//! Or le repli n'est poussé qu'aux **trois** sites de `orchestrator.rs` qui
//! portent la double garde `device_id.starts_with("local:")` +
//! `downcast_ref::<LocalOutput>()`, et il n'est exécuté que par `LocalOutput`.
//! `transcode_source_to_file` — la seule porte serveur qui traite le PCM
//! destiné à un renderer réseau — prend `eq`, `convolver`, `replaygain`, et
//! rien d'autre.
//!
//! Sur une zone réseau : accepté, persisté, relu… et **sans effet**.
//!
//! Le chemin du signal, lui, disait déjà la vérité (`zone_mono_downmix_step`
//! rend `None` hors `output_type == "local"`, et `None` en mode PURE) : c'est
//! le **réglage** qui se taisait, pas l'affichage de la chaîne.
//!
//! ## Ce que ce fichier cloue
//!
//! Les DEUX sens, sur les deux surfaces atteignables en HTTP :
//!
//! - une zone **locale** applique le repli et n'annonce RIEN — le témoin, sans
//!   lequel une règle qui crierait « indisponible » partout passerait ;
//! - une zone **réseau** annonce l'indisponibilité, avec son motif, **et garde
//!   sa valeur persistée** : refuser l'écriture serait l'autre correction, et
//!   ce serait une régression (une zone change de sortie).
//!
//! Les tests passent par le VRAI routeur (`tune_server::routes::router`) : la
//! règle n'est recopiée nulle part ici.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

fn app() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn patch(app: &axum::Router, path: &str, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::builder()
            .method("PATCH")
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// Crée une zone du type demandé et rend son id.
///
/// ⚠️ L'identifiant de périphérique n'est pas décoratif : c'est le préfixe
/// `local:` que les trois sites d'installation interrogent, et donc celui que
/// la règle interroge. Une zone `local` sans périphérique serait une zone
/// orpheline — un autre cas, couvert plus bas.
async fn creer_zone(app: &axum::Router, nom: &str, output_type: &str, device: Option<&str>) -> i64 {
    let corps = json!({
        "name": nom,
        "output_type": output_type,
        "output_device_id": device,
    });
    let (status, body) = envoyer(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/zones")
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "création de zone : {body}");
    body["id"].as_i64().expect("un id de zone")
}

/// Une zone LOCALE avec un périphérique `local:` assigné.
///
/// ⚠️ Écrite en base plutôt que par `POST /zones` : la route de création refuse
/// un `local:…` qui ne correspond à aucune carte son présente (« Local audio
/// device not found »), et Shrek n'en a pas. Ce que ces tests éprouvent est
/// `GET`/`PATCH /zones/{id}`, pas la validation de la création ; la ligne écrite
/// ici est exactement celle que la route lirait sur la machine d'un
/// utilisateur. Même procédé que les tests de `zones.rs` (`local_zone_migrated`)
/// et que `openhome_pins_atteignent_le_renderer.rs`.
fn zone_locale(state: &AppState) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create("Bureau", Some("local"), Some("local:dac-3254"))
        .expect("création de la zone locale")
}

async fn zone_reseau(app: &axum::Router) -> i64 {
    creer_zone(app, "Salon DLNA", "dlna", Some("uuid-renderer-3254")).await
}

fn statut<'a>(charge: &'a Value, route: &str) -> &'a Value {
    let s = &charge["mono_downmix_status"];
    assert!(
        !s.is_null(),
        "{route} : la fiche de zone publie `mono_downmix` mais pas \
         `mono_downmix_status` — le réglage est de nouveau muet sur ce qu'il \
         vaut (#3254). Charge utile : {charge}"
    );
    s
}

/// Sur une zone où le repli AGIT, le serveur ne doit rien annoncer du tout.
fn exiger_le_silence(charge: &Value, route: &str, demande: bool) {
    let s = statut(charge, route);
    assert_eq!(
        s["requested"].as_bool(),
        Some(demande),
        "{route} : `requested` doit refléter la valeur persistée. {s}"
    );
    assert_eq!(
        s["effective"].as_bool(),
        Some(demande),
        "{route} : sur une sortie LOCALE hors PURE, le repli agit exactement \
         comme il est demandé — c'est le témoin du cas nominal (#3254). {s}"
    );
    assert_eq!(
        s["unavailable"].as_bool(),
        Some(false),
        "{route} : une zone locale ne doit RIEN verrouiller. {s}"
    );
    assert!(
        s["reason"].is_null(),
        "{route} : un motif est publié pour une zone où le repli agit — \
         l'écran griserait un contrôle qui marche. {s}"
    );
    assert!(s["detail"].is_null(), "{route} : {s}");
}

/// Sur une zone où le repli n'a aucun chemin, le serveur doit le DIRE.
fn exiger_l_aveu(charge: &Value, route: &str, motif: &str, demande: bool) {
    let s = statut(charge, route);
    assert_eq!(
        s["requested"].as_bool(),
        Some(demande),
        "{route} : la demande de l'utilisateur ne doit pas être effacée. {s}"
    );
    assert_eq!(
        s["effective"].as_bool(),
        Some(false),
        "{route} : rien n'applique le repli ici. {s}"
    );
    assert_eq!(
        s["unavailable"].as_bool(),
        Some(true),
        "{route} : `unavailable` doit VERROUILLER le contrôle, y compris case \
         décochée — sinon l'utilisateur coche, puis découvre (#3254). {s}"
    );
    assert_eq!(
        s["reason"].as_str(),
        Some(motif),
        "{route} : le code du motif est stable et destiné à la machine. {s}"
    );
    let detail = s["detail"].as_str().unwrap_or("");
    assert!(
        detail.len() > 40,
        "{route} : `detail` doit EXPLIQUER en clair — un écran sans table de \
         traduction n'a que lui. {s}"
    );
}

/// ⭐ LE TÉMOIN — une zone locale applique le repli, sans rien annoncer.
///
/// Le cas nominal ne bouge pas d'un bit : ni verrou, ni motif, ni libellé.
#[tokio::test]
async fn le_temoin_une_zone_locale_applique_le_repli_sans_rien_annoncer() {
    let (app, state) = app();
    let id = zone_locale(&state);

    // Désarmé au départ : rien à annoncer non plus.
    let (status, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_le_silence(&fiche, "GET /zones/{id} (désarmé)", false);

    // Armé : le repli agit, et le serveur se tait.
    let (status, fiche) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({ "mono_downmix": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    assert_eq!(fiche["mono_downmix"].as_bool(), Some(true), "{fiche}");
    exiger_le_silence(&fiche, "PATCH /zones/{id}", true);

    let (_, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    exiger_le_silence(&fiche, "GET /zones/{id} (armé)", true);
}

/// Une zone réseau annonce que le repli n'agira pas — à l'écriture ET à la
/// relecture, puisque `PATCH` rend la fiche.
#[tokio::test]
async fn une_zone_reseau_annonce_que_le_repli_nagira_pas() {
    let (app, _state) = app();
    let id = zone_reseau(&app).await;

    let (status, fiche) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({ "mono_downmix": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_l_aveu(&fiche, "PATCH /zones/{id}", "non_local_output", true);

    let (_, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    exiger_l_aveu(&fiche, "GET /zones/{id}", "non_local_output", true);
}

/// Le verrou ne dépend pas de la case : une zone réseau JAMAIS touchée dit déjà
/// que le contrôle n'a pas de sens chez elle.
#[tokio::test]
async fn la_relecture_verrouille_le_controle_meme_case_decochee() {
    let (app, _state) = app();
    let id = zone_reseau(&app).await;
    let (status, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_l_aveu(&fiche, "GET /zones/{id}", "non_local_output", false);
}

/// La liste des zones porte la même vérité que la fiche : c'est elle que le
/// client charge en premier, et deux surfaces qui divergent en valent zéro.
#[tokio::test]
async fn la_liste_des_zones_porte_le_meme_statut_que_la_fiche() {
    let (app, state) = app();
    let locale = zone_locale(&state);
    let reseau = zone_reseau(&app).await;
    for id in [locale, reseau] {
        patch(
            &app,
            &format!("/api/v1/zones/{id}"),
            json!({ "mono_downmix": true }),
        )
        .await;
    }

    let (status, liste) = get(&app, "/api/v1/zones").await;
    assert_eq!(status, StatusCode::OK, "{liste}");
    let trouver = |zone_id: i64| {
        liste
            .as_array()
            .expect("GET /zones rend un tableau")
            .iter()
            .find(|z| z["id"].as_i64() == Some(zone_id))
            .unwrap_or_else(|| panic!("zone {zone_id} absente de GET /zones : {liste}"))
            .clone()
    };
    exiger_le_silence(&trouver(locale), "GET /zones (locale)", true);
    exiger_l_aveu(
        &trouver(reseau),
        "GET /zones (réseau)",
        "non_local_output",
        true,
    );
}

/// Le mode PURE désarme le repli sur une zone LOCALE, et doit le dire.
///
/// `zone_mono_downmix_with` rend `false` sans même lire la clé quand la zone est
/// en PURE : sommer les deux voies réécrirait chaque échantillon. La fiche, elle,
/// publie `mono_downmix` à sa valeur PERSISTÉE — délibérément, c'est l'état de
/// l'interrupteur. Sans le statut, cet écart-là aussi était muet.
#[tokio::test]
async fn le_mode_pure_annonce_quil_desarme_la_sortie_mono() {
    let (app, state) = app();
    let id = zone_locale(&state);
    patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({ "mono_downmix": true }),
    )
    .await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(&format!("zone_{id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();

    let (status, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_l_aveu(&fiche, "GET /zones/{id} (PURE)", "pure_mode", true);
}

/// Une zone `local` SANS périphérique est orpheline : aucune chaîne locale ne
/// tourne pour elle, donc rien n'appliquera le repli. Le statut doit le dire,
/// et par le motif qui reste vrai — la zone n'a pas de sortie locale.
///
/// Ce cas garde le prédicat lui-même : une règle écrite sur `output_type ==
/// "local"` plutôt que sur le préfixe `local:` du périphérique — celui que les
/// trois sites interrogent — passerait au vert ici en mentant.
#[tokio::test]
async fn une_zone_locale_orpheline_nannonce_pas_un_repli_quelle_na_pas() {
    let (app, _state) = app();
    let id = creer_zone(&app, "Orpheline", "local", None).await;
    patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({ "mono_downmix": true }),
    )
    .await;
    let (status, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_l_aveu(
        &fiche,
        "GET /zones/{id} (orpheline)",
        "non_local_output",
        true,
    );
}

/// ⭐ CONTRE-ÉPREUVE — la « correction » qui consisterait à REFUSER l'écriture
/// sur une zone réseau est interdite.
///
/// Une zone change de sortie. Effacer ou refuser la valeur ferait perdre le
/// réglage au moment où la zone repasse en local — exactement le prix déjà payé
/// par #1786 (crossfeed amnésique).
#[tokio::test]
async fn le_reglage_reste_persiste_sur_une_zone_reseau() {
    let (app, state) = app();
    let id = zone_reseau(&app).await;
    let (status, fiche) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({ "mono_downmix": true }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "le PATCH ne doit pas être refusé : dire « ça n'agit pas » n'est pas \
         « je n'enregistre pas » (#3254). {fiche}"
    );
    assert_eq!(
        fiche["mono_downmix"].as_bool(),
        Some(true),
        "la valeur publiée doit rester la valeur PERSISTÉE. {fiche}"
    );
    assert_eq!(
        SettingsRepo::with_backend(state.backend.clone())
            .get(&format!("zone_{id}_mono_downmix"))
            .unwrap()
            .as_deref(),
        Some("true"),
        "la clé de réglage doit être écrite même sur une zone réseau"
    );

    // Et elle redevient vivante si la zone repasse sur une sortie locale.
    let (_, fiche) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({ "output_type": "local", "output_device_id": "local:dac-3254" }),
    )
    .await;
    exiger_le_silence(&fiche, "PATCH /zones/{id} (retour en local)", true);
}

/// ⭐ Le caractère ADDITIF : la fiche de zone n'a rien perdu.
///
/// Un client qui ne lit pas `mono_downmix_status` doit voir exactement l'écran
/// d'avant.
#[tokio::test]
async fn les_champs_historiques_de_la_fiche_de_zone_sont_intacts() {
    let (app, _state) = app();
    let id = zone_reseau(&app).await;
    let (status, fiche) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    for cle in [
        "id",
        "name",
        "output_type",
        "output_device_id",
        "mono_downmix",
        "brand",
        "model",
        "gain_trim_db",
        "upnp_renderer",
        "upnp_silence",
        "signal_path",
        "online",
    ] {
        assert!(
            fiche.get(cle).is_some(),
            "`{cle}` a disparu de la fiche de zone — l'ajout de \
             `mono_downmix_status` devait être strictement additif (#3254). {fiche}"
        );
    }
}
