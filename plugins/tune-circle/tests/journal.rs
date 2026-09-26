//! Témoin 8 (#5018) : ni jeton ni adresse mail au journal.
//!
//! Un binaire à lui seul, un seul test, un abonné GLOBAL posé avant tout le
//! reste : `tracing` met en cache pour tout le processus l'intérêt de chaque
//! point d'appel, et un abonné local posé au milieu d'une suite parallèle
//! perdrait des évènements au hasard (leçon de #2665 et #2890). Niveau TRACE,
//! toutes cibles : ce qu'un journal le plus bavard écrirait.

mod commun;

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::json;

use commun::*;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn ni_jeton_ni_courriel_au_journal() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .init();

    // Tout le parcours, rafraîchissement et pannes compris.
    let faux = demarrer().await;
    faux.etat.lock().unwrap().invitations_permises = 1;
    let app = app(base(&faux.base, Some(JETON_PERIME)));
    appel(&app, "GET", "/", None).await; // 401 → rafraîchi → 200
    appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": COURRIEL_INVITE })),
    )
    .await;
    appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": COURRIEL_INVITE })),
    )
    .await; // 429
    appel(&app, "POST", "/invitations/21/accept", None).await;
    faux.etat.lock().unwrap().invitations_permises = 5;
    appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": COURRIEL_MEMBRE })),
    )
    .await; // 409
    appel(&app, "DELETE", "/members/9", None).await;
    appel(&app, "GET", "/", None).await; // liste avec `sent` porteur d'adresses
    faux.etat.lock().unwrap().panne = true;
    appel(&app, "GET", "/", None).await;
    let app_morte = app_sur(&adresse_morte().await);
    appel(&app_morte, "GET", "/", None).await;

    let journal = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    // Contre-épreuve de la capture elle-même : un journal vide passerait tout.
    for attendu in [
        "circle_relai",
        "circle_session_rafraichie",
        "circle_cloud_en_panne",
        "circle_cloud_injoignable",
    ] {
        assert!(
            journal.contains(attendu),
            "évènement {attendu} absent de la capture"
        );
    }
    for secret in [
        JETON,
        JETON_PERIME,
        JETON_NEUF,
        RAFRAICHISSEMENT,
        RAFRAICHISSEMENT_NEUF,
        "SECRET-5018",
        COURRIEL_INVITE,
        COURRIEL_DEJA_INVITE,
        COURRIEL_MEMBRE,
        "@exemple.fr",
    ] {
        assert!(
            !journal.contains(secret),
            "le journal contient {secret:?} :\n{journal}"
        );
    }
}

fn app_sur(url: &str) -> axum::Router {
    app(base(url, Some(JETON)))
}
