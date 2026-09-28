//! Tune Circle, décisions de Bertrand du 28/09/2026 (portées dans #5326) :
//!
//! 1. la déconnexion SSO efface AUSSI le jeton de liaison du serveur au compte
//!    (`cloud_server_link_token`, T2 #5325) ;
//! 2. AVANT d'effacer les jetons, elle défait la liaison chez le cloud :
//!    `DELETE {mozaik}/api/v1/cloud-library/{server_id}/link`, jeton OAuth et
//!    `X-Tune-Server-Token`. Un échec (réseau, 404, 401) est journalisé sans
//!    jeton et n'empêche PAS la déconnexion locale.
//!
//! Sans (1), un serveur déconnecté puis reconnecté à un AUTRE compte
//! présenterait encore, sur chaque `sync`, le jeton délivré au premier. La
//! liaison se refait à la connexion suivante (`sso_callback`).
//!
//! Le faux mozaiklabs est un vrai serveur axum sur une socket locale.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use tower::ServiceExt;
use tune_core::cloud::library_sync::{CLE_JETON_DE_LIAISON, serveur_lie};
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const ACCES: &str = "jeton-acces-SECRET-5326";
const RAFRAICHISSEMENT: &str = "jeton-rafraichissement-SECRET-5326";
const LIAISON: &str = "jeton-liaison-SECRET-5326";
const SERVEUR: &str = "srv-5326";

/// Ce que le faux cloud a reçu sur `DELETE …/link` : (server_id, Authorization,
/// X-Tune-Server-Token).
type Recus = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

/// Un faux mozaiklabs dont `DELETE /api/v1/cloud-library/{server_id}/link`
/// rend `statut`.
async fn faux_cloud(statut: StatusCode) -> (String, Recus) {
    let recus: Recus = Arc::default();
    let app = axum::Router::new()
        .route(
            "/api/v1/cloud-library/{server_id}/link",
            axum::routing::delete(
                move |State(r): State<Recus>, Path(id): Path<String>, h: HeaderMap| async move {
                    let en_tete =
                        |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
                    r.lock().unwrap().push((
                        id,
                        en_tete("authorization"),
                        en_tete("x-tune-server-token"),
                    ));
                    (statut, axum::Json(serde_json::json!({ "error": "x" })))
                },
            ),
        )
        .with_state(recus.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), recus)
}

/// Une adresse où rien n'écoute.
async fn adresse_morte() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    drop(ecoute);
    format!("http://{adresse}")
}

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Un serveur connecté et lié, dont le cloud est à `base`.
fn serveur_lie_a(base: &str) -> (AppState, tune_core::test_scratch::ScratchDir) {
    let dir = tune_core::test_scratch::scratch_dir("tune-i5326-sso");
    let db = dir.join("library.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base).unwrap();
    settings.set("mozaik_access_token", ACCES).unwrap();
    settings
        .set("mozaik_refresh_token", RAFRAICHISSEMENT)
        .unwrap();
    settings.set("server_id", SERVEUR).unwrap();
    settings.set(CLE_JETON_DE_LIAISON, LIAISON).unwrap();
    assert!(serveur_lie(&settings), "témoin : le serveur est lié avant");
    (state, dir)
}

/// `POST /cloud/sso/disconnect`, journal capturé à tous les niveaux.
async fn se_deconnecter(state: &AppState) -> (StatusCode, String) {
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);
    let reponse = tune_server::routes::router(state.clone())
        .oneshot(
            Request::post("/api/v1/cloud/sso/disconnect")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    (reponse.status(), journal.texte())
}

/// La déconnexion a effacé, localement, la session ET la liaison.
fn tout_est_efface(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(
        settings.get("mozaik_access_token").ok().flatten(),
        None,
        "témoin : la session SSO est effacée"
    );
    assert_eq!(
        settings.get(CLE_JETON_DE_LIAISON).ok().flatten(),
        None,
        "la déconnexion SSO doit effacer `{CLE_JETON_DE_LIAISON}` : le jeton de \
         liaison survivrait au compte qui l'a obtenu"
    );
    assert!(!serveur_lie(&settings));
    // Le `server_id` reste : c'est l'identité du SERVEUR, pas du compte.
    assert_eq!(
        settings.get("server_id").ok().flatten().as_deref(),
        Some(SERVEUR)
    );
}

fn aucun_jeton_dans(journal: &str) {
    for secret in [ACCES, RAFRAICHISSEMENT, LIAISON] {
        assert!(
            !journal.contains(secret),
            "un jeton est au journal ({secret}) :\n{journal}"
        );
    }
}

#[tokio::test]
async fn la_deconnexion_delie_le_serveur_chez_le_cloud_puis_efface_le_jeton() {
    let (base, recus) = faux_cloud(StatusCode::OK).await;
    let (state, _dir) = serveur_lie_a(&base);

    let (statut, journal) = se_deconnecter(&state).await;
    assert_eq!(statut, StatusCode::OK);

    // L'appel est parti AVANT l'effacement : il porte les deux jetons, que
    // seuls les réglages d'avant la déconnexion connaissaient.
    assert_eq!(
        *recus.lock().unwrap(),
        vec![(
            SERVEUR.to_string(),
            Some(format!("Bearer {ACCES}")),
            Some(LIAISON.to_string()),
        )],
        "la déconnexion doit défaire la liaison chez le cloud, avec l'OAuth et \
         `X-Tune-Server-Token`, avant d'effacer les jetons"
    );
    tout_est_efface(&state);
    aucun_jeton_dans(&journal);
}

#[tokio::test]
async fn un_refus_du_cloud_n_empeche_pas_la_deconnexion() {
    for refus in [
        StatusCode::NOT_FOUND,
        StatusCode::UNAUTHORIZED,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let (base, recus) = faux_cloud(refus).await;
        let (state, _dir) = serveur_lie_a(&base);
        let (statut, journal) = se_deconnecter(&state).await;
        assert_eq!(statut, StatusCode::OK, "{refus}");
        assert_eq!(recus.lock().unwrap().len(), 1, "{refus}");
        tout_est_efface(&state);
        assert!(
            journal.contains("cloud_server_unlink_failed")
                && journal.contains(&format!("HTTP {}", refus.as_u16())),
            "l'échec est journalisé, par son statut : {journal}"
        );
        aucun_jeton_dans(&journal);
    }
}

#[tokio::test]
async fn un_cloud_injoignable_n_empeche_pas_la_deconnexion() {
    let base = adresse_morte().await;
    let (state, _dir) = serveur_lie_a(&base);
    let (statut, journal) = se_deconnecter(&state).await;
    assert_eq!(statut, StatusCode::OK);
    tout_est_efface(&state);
    assert!(journal.contains("cloud_server_unlink_failed"), "{journal}");
    aucun_jeton_dans(&journal);
}

#[tokio::test]
async fn sans_jeton_de_liaison_rien_ne_part() {
    let (base, recus) = faux_cloud(StatusCode::OK).await;
    let (state, _dir) = serveur_lie_a(&base);
    SettingsRepo::with_backend(state.backend.clone())
        .delete(CLE_JETON_DE_LIAISON)
        .unwrap();
    let (statut, _journal) = se_deconnecter(&state).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(recus.lock().unwrap().is_empty(), "rien à défaire");
    tout_est_efface(&state);
}
