//! #8006 — `/diagnostics/oaat` gardait le registre des sorties pendant qu'il
//! attendait le verrou de chacune. Une sortie occupée (démarrage d'un titre
//! qui traîne) figeait alors le registre, donc la lecture de TOUTES les
//! zones, jusqu'à ce qu'elle se libère.
//!
//! Le banc tient lui-même le verrou d'une sortie factice, comme un
//! `play_media` qui n'en finit pas, et vérifie que le registre reste libre
//! pendant que la route travaille, puis qu'elle répond.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::time::Duration;
use tower::ServiceExt;
use tune_core::outputs::mock::MockOutput;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_sortie_occupee_ne_fige_plus_le_registre_8006() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let sortie = {
        let mut registre = state.outputs.lock().await;
        registre.register(Box::new(MockOutput::new("devialet-8006", "Devialet")));
        registre.get("devialet-8006").unwrap()
    };
    let app = crate::routes::router(state.clone());
    let occupee = sortie.lock().await;

    let requete = tokio::spawn(async move {
        app.oneshot(
            Request::get("/api/v1/system/diagnostics/oaat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    });
    // Le temps que la route atteigne la sortie occupée.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let registre_libre = tokio::time::timeout(Duration::from_secs(1), state.outputs.lock())
        .await
        .is_ok();
    assert!(
        registre_libre,
        "le registre des sorties doit rester libre pendant que le diagnostic \
         attend une sortie occupée, sinon toutes les zones attendent avec lui \
         (#8006)"
    );

    let statut = tokio::time::timeout(Duration::from_secs(5), requete)
        .await
        .expect("le diagnostic doit répondre malgré la sortie occupée")
        .unwrap();
    assert_eq!(statut, StatusCode::OK);
    drop(occupee);
}
