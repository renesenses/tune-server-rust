//! #7614 — témoins des routes du suréchantillonneur.
use super::*;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("état")
}

#[tokio::test]
async fn le_catalogue_donne_chaque_filtre_et_sa_reserve() {
    let state = etat();
    let Json(v) = catalogue(State(state)).await;
    let filtres = v["filters"].as_array().unwrap();
    assert_eq!(filtres.len(), greffon::OFFERED_FILTERS.len());
    for f in filtres {
        for (_, db) in f["headroom_db"].as_object().unwrap() {
            assert!(db.as_f64().unwrap() <= 0.0, "{f}");
        }
    }
    assert_eq!(v["default_filter"], "polyvalent");
    assert_eq!(v["premium"], false);
    assert_eq!(v["installed"], false);
    assert!(v["machine_test"].is_null());
}

#[tokio::test]
async fn activer_sans_premium_est_refuse_402() {
    let state = etat();
    let r = ecrire_zone(
        State(state.clone()),
        Path(1),
        HeaderMap::new(),
        Json(json!({"enabled": true})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PAYMENT_REQUIRED);
    // Rien n'a été écrit : la zone reste désactivée.
    let Json(v) = lire_zone(State(state), Path(1)).await;
    assert_eq!(v["settings"]["enabled"], false);
    assert_eq!(v["constraint"], "disabled");
    assert_eq!(v["applies_at"], "next_track");
}

#[tokio::test]
async fn desactiver_est_toujours_permis_et_un_reglage_invalide_refuse() {
    let state = etat();
    let r = ecrire_zone(
        State(state.clone()),
        Path(2),
        HeaderMap::new(),
        Json(json!({"enabled": false, "filter": "precision-pm", "max_ratio": 4})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let Json(v) = lire_zone(State(state.clone()), Path(2)).await;
    assert_eq!(v["settings"]["filter"], "precision-pm");
    assert_eq!(v["settings"]["max_ratio"], 4);
    for refuse in [
        json!({"enabled": false, "filter": "precision-longue"}),
        json!({"enabled": false, "max_ratio": 3}),
    ] {
        let r = ecrire_zone(
            State(state.clone()),
            Path(2),
            HeaderMap::new(),
            Json(refuse.clone()),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY, "{refuse}");
    }
}
