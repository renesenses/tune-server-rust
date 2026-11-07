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

/// Étape I5 : au PREMIER enregistrement d'un réglage activé, la machine est
/// mesurée (hors du fil asynchrone) et le verdict gardé ; le réglage
/// s'applique alors à la piste suivante, sans autre contrainte.
#[tokio::test]
async fn premier_usage_mesure_la_machine_puis_le_reglage_s_applique() {
    let state = etat();
    state.license.set_account_premium(true, None).await;
    let s = settings(&state);
    s.set(&format!("plugin_{}_installed", su::ID), "true")
        .unwrap();
    s.set(&format!("plugin_{}_enabled", su::ID), "true")
        .unwrap();
    assert!(su::verdict_enregistre(&s).is_none());
    let r = ecrire_zone(
        State(state.clone()),
        Path(3),
        HeaderMap::new(),
        Json(json!({"enabled": true, "filter": "resolution"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let verdict = su::verdict_enregistre(&s).expect("le premier usage mesure la machine");
    assert!(!verdict.results.is_empty());
    assert!(
        verdict
            .results
            .iter()
            .all(|m| greffon::OFFERED_FILTERS.contains(&m.filter.as_str())),
        "seuls les filtres offerts sont mesurés"
    );
    let Json(v) = lire_zone(State(state), Path(3)).await;
    assert_eq!(v["settings"]["enabled"], true);
    // Shrek tient le filtre court partout ; une machine qui ne le tiendrait
    // pas verrait une contrainte nulle aussi (le filtre non admis se lit dans
    // `admitted_filters`, la cadence est écartée à l'ouverture).
    assert!(v["constraint"].is_null(), "{v}");
}
