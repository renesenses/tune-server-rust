//! #8521 (Tades, fils 2791 et 2792, rc11) — « écrire les métadonnées dans les
//! fichiers » et « paroles en ligne » retrouvés décochés au matin, sans
//! redémarrage, sur deux serveurs.
//!
//! Hypothèse écartée ici : une AUTRE écriture de réglages (n'importe quel
//! champ de l'écran, une application, un enregistrement automatique) qui
//! remettrait ces deux clés au défaut parce qu'elles sont absentes de la
//! requête. `PATCH /system/config` reçoit une carte JSON brute
//! (`ConfigPatch(serde_json::Map)`), sans structure ni `#[serde(default)]` :
//! il n'écrit que les clés présentes. Ces témoins le tiennent sur la vraie
//! route, base relue après coup.
use crate::state::AppState;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

const ECRITURE: &str = tune_core::metadata::ecriture_fichiers::CLE;
const PAROLES: &str = tune_core::library::lyrics_pass::SETTING_LRCLIB_ENABLED;

async fn appel(state: &AppState, methode: &str, corps: Option<Value>) -> (StatusCode, Value) {
    let mut requete = Request::builder()
        .method(methode)
        .uri("/api/v1/system/config");
    let corps = match corps {
        Some(c) => {
            requete = requete.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = crate::routes::router(state.clone())
        .oneshot(requete.body(corps).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn consentis() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set(ECRITURE, "true").unwrap();
    reglages.set(PAROLES, "true").unwrap();
    state
}

fn en_base(state: &AppState, cle: &str) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(cle)
        .unwrap()
}

/// Les autres cases de la même carte, et un réglage d'un autre écran : aucune
/// ne touche aux deux consentements, ni en base ni dans `GET /system/config`.
#[tokio::test]
async fn un_patch_d_autres_reglages_ne_touche_pas_aux_consentements() {
    let state = consentis();
    for corps in [
        json!({ "quality_split": false }),
        json!({ "scan_import_playlists": false, "enrich_on_scan": false }),
        json!({ "zone_auto_create": true }),
    ] {
        let (statut, reponse) = appel(&state, "PATCH", Some(corps.clone())).await;
        assert_eq!(statut, StatusCode::OK, "{corps} → {reponse}");
        for cle in [ECRITURE, PAROLES] {
            assert_eq!(
                en_base(&state, cle).as_deref(),
                Some("true"),
                "{cle} réécrit par un PATCH qui ne le portait pas : {corps}"
            );
        }
    }
    let (statut, config) = appel(&state, "GET", None).await;
    assert_eq!(statut, StatusCode::OK);
    for cle in [ECRITURE, PAROLES] {
        let v = &config[cle];
        assert!(
            v == &json!(true) || v == &json!("true"),
            "GET /system/config publie {cle} = {v} après des PATCH qui ne le portaient pas"
        );
    }
}

/// Un corps vide n'écrit rien.
#[tokio::test]
async fn un_patch_vide_n_ecrit_rien() {
    let state = consentis();
    let (statut, reponse) = appel(&state, "PATCH", Some(json!({}))).await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(en_base(&state, ECRITURE).as_deref(), Some("true"));
    assert_eq!(en_base(&state, PAROLES).as_deref(), Some("true"));
}

/// Contre-épreuve : la clé PRÉSENTE est bien écrite, et seule elle.
#[tokio::test]
async fn seule_la_cle_presente_est_ecrite() {
    let state = consentis();
    let (statut, reponse) = appel(&state, "PATCH", Some(json!({ PAROLES: false }))).await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(en_base(&state, PAROLES).as_deref(), Some("false"));
    assert_eq!(
        en_base(&state, ECRITURE).as_deref(),
        Some("true"),
        "décocher les paroles ne doit pas décocher l'écriture dans les fichiers"
    );
}
