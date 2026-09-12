//! #3664 — le refus qui propose une station laisse une TRACE NOMMÉE.
//!
//! ## Pourquoi un binaire d'essai à lui tout seul
//!
//! Deux raisons, et la première est une mesure, pas une préférence.
//!
//! 1. Cet essai ne lit pas une réponse HTTP : il lit le JOURNAL. Il installe
//!    donc un abonné `tracing` sur son fil, et l'intérêt d'un point d'émission
//!    est mis en cache GLOBALEMENT au premier passage. Dans un binaire où
//!    d'autres essais tournent en parallèle sans abonné, ce cache peut figer
//!    le point d'émission sur « jamais » : l'essai devient vert ou rouge selon
//!    l'ordre d'ordonnancement. Mesuré le 11/09/2026 — vert seul, ROUGE avec
//!    les neuf autres essais de `radio_adresse_pas_un_flux_3578.rs`. Un faux
//!    rouge qu'on rejoue est un faux rouge qu'on finit par ignorer.
//! 2. Ce qu'il garde est d'une autre nature que les gardes de comportement :
//!    il tient l'OBSERVABILITÉ du correctif, pas son effet.
//!
//! ## Ce qu'il garde
//!
//! Le correctif de #3664 livré en v0.9.145 (PR #3743) n'émettait aucune trace
//! nommée. L'audit du 10/09/2026 en a tiré la conséquence : il est
//! **invérifiable dans un binaire publié**. Les deux jetons candidats ne
//! séparent rien — la clef JSON de suggestion vaut **94 en v0.9.144 comme en
//! v0.9.145** sur le binaire x86_64 (91/91 sur aarch64, le mot sert dans les
//! métadonnées), et le marqueur du refus vaut **2 dans les deux**, la ligne
//! existant bien avant la suggestion.
//!
//! Cet essai ne vérifie PAS que la clef figure dans la source — un `grep` le
//! ferait, et se trouverait lui-même. Il fait tourner la route réelle et lit
//! ce qui SORT. C'est la différence entre « le marqueur est écrit » et « le
//! marqueur est BRANCHÉ ».
//!
//! L'aiguille est assemblée à l'exécution, morceau par morceau, pour qu'elle
//! ne figure nulle part d'un seul tenant dans ce fichier.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Un serveur de station qui rend une PAGE WEB — le cas du fil 1698.
async fn station_qui_rend_une_page() -> String {
    let app = axum::Router::new().route(
        "/page",
        get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "text/html; charset=UTF-8")],
                "<html><body>Ecoutez-nous ici</body></html>",
            )
                .into_response()
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}")
}

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn creer(app: &axum::Router, nom: &str, url: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post("/api/v1/radios")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"name": nom, "stream_url": url}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Le journal capté pendant l'exécution.
#[derive(Clone, Default)]
struct JournalCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

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

#[tokio::test]
async fn le_refus_qui_propose_une_station_laisse_une_trace_nommee() {
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let (app, _state) = app_et_etat();
    let station = station_qui_rend_une_page().await;
    // Le nom saisi par Belkadi Yacine, et une page web à la place du flux.
    let (status, corps) = creer(&app, "Radio Paradise", &format!("{station}/page")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");

    let ecrit = String::from_utf8(journal.0.lock().unwrap().clone()).expect("journal utf-8");
    // Assemblée ici, jamais écrite d'un bloc.
    let aiguille = ["radio", "refus", "suggestion", "catalogue", "3664"].join("_");
    assert!(
        ecrit.contains(&aiguille),
        "le chemin du refus doit laisser une trace NOMMEE, faute de quoi le correctif reste inverifiable dans un binaire publie. Journal capte : {ecrit}"
    );

    let compte = ecrit
        .split(&aiguille)
        .nth(1)
        .and_then(|apres| apres.split("proposees=").nth(1))
        .and_then(|apres| apres.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    assert!(
        !compte.is_empty() && compte != "0",
        "la trace doit dire COMBIEN de stations ont ete proposees : sans chiffre, elle ne distingue pas « le serveur sait proposer » de « le serveur a propose quelque chose ». Compte lu : « {compte} ». Journal capte : {ecrit}"
    );

    // ---------------------------------------------------------------------
    // CONTRE-EPREUVE, dans le MEME essai : la trace ne doit pas se contenter
    // d'etre la, elle doit dire la verite. Un refus que rien ne rapproche du
    // catalogue laisse la meme ligne, avec un compte a ZERO. Sans cela, une
    // trace qui annoncerait toujours le meme chiffre passerait la garde
    // ci-dessus.
    //
    // Les deux mesures vivent dans UN SEUL essai a dessein : deux essais
    // paralleles installant chacun leur abonne se disputent le cache
    // d'interet de `tracing`, et le second devient rouge selon l'ordre
    // d'ordonnancement. Un seul fil, un seul verdict.
    // ---------------------------------------------------------------------
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let (app, _state) = app_et_etat();
    // Adresse malformee (aucun hote a comparer) et nom qu'aucune station du
    // catalogue livre ne porte.
    let (status, corps) = creer(&app, "Zzyxwv", "http;//zzyxwv.example.net/flux").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");

    let ecrit = String::from_utf8(journal.0.lock().unwrap().clone()).expect("journal utf-8");
    let compte = ecrit
        .split(&aiguille)
        .nth(1)
        .and_then(|apres| apres.split("proposees=").nth(1))
        .and_then(|apres| apres.split_whitespace().next())
        .unwrap_or("");
    assert_eq!(
        compte, "0",
        "rien ne rapproche cette saisie du catalogue : la trace doit dire zero, pas un chiffre de complaisance. Journal capte : {ecrit}"
    );
}
