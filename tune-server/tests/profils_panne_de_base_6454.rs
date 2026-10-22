//! `GET /profiles` sur une base en échec : une erreur, pas une liste vide
//! (#6454).
//!
//! Le défaut : `list_profiles` répondait `200 []` quand `repo.list()`
//! échouait. Le client web lisait « aucun profil », tentait de créer
//! « Default » et, si la création échouait aussi, l'Accueil restait sur
//! « Chargement… » sans erreur visible. Son propre commentaire disait déjà
//! « Don't swallow a DB error as an empty list ».
//!
//! L'essai tient trois propriétés :
//!
//! 1. sur une base saine, la route rend `200` et une liste ;
//! 2. sur une base en échec, elle rend un `5xx` portant le code
//!    `profils_illisibles`, et non un tableau ;
//! 3. la panne laisse une ligne `ERROR` au journal, qui nomme la route.
//!
//! Abonné `tracing` global et un seul essai dans ce binaire : voir
//! `panne_sql_journalisee.rs` pour la raison.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn lister_les_profils(app: &axum::Router) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::get("/api/v1/profiles")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, corps)
}

#[tokio::test]
async fn une_base_en_echec_rend_une_erreur_journalisee_et_non_une_liste_vide() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("base en mémoire");
    let app = tune_server::routes::router(etat.clone());

    // 1. Base saine : une liste.
    let (statut, corps) = lister_les_profils(&app).await;
    assert_eq!(statut, StatusCode::OK, "base saine, corps = {corps}");
    assert!(corps.is_array(), "base saine, corps = {corps}");

    // 2. La table des profils devient illisible : `repo.list()` rend un `Err`,
    // comme sur un schéma en retard ou une base verrouillée. Un renommage
    // plutôt qu'un DROP : d'autres tables y font référence.
    etat.backend
        .execute_batch("ALTER TABLE profiles RENAME TO profiles_hors_service")
        .expect("la table des profils doit pouvoir être mise hors service");

    let (statut, corps) = lister_les_profils(&app).await;
    assert!(
        statut.is_server_error(),
        "une base en échec doit rendre un 5xx, pas {statut} (corps = {corps}) : \
         un `200 []` fait croire au client qu'il n'y a aucun profil"
    );
    assert!(
        !corps.is_array(),
        "une base en échec ne doit pas rendre une liste, corps = {corps}"
    );
    assert_eq!(corps["code"], "profils_illisibles", "corps = {corps}");

    // 3. La panne est au journal.
    let journal = capture.texte();
    assert!(
        journal.contains("ERROR") && journal.contains("list_profiles"),
        "la panne de `GET /profiles` doit laisser une ligne ERROR, journal = {journal}"
    );
}
