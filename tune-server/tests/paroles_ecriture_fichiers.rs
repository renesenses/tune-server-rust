//! Garde-fou : l'écriture des paroles dans les fichiers de l'utilisateur ne
//! part que sur un geste ET un consentement (issue #2172).
//!
//! ## Pourquoi ce fichier
//!
//! v0.9.118 a livré la moitié de l'issue #2172 — l'indicateur de couverture et
//! la passe de fond LRCLIB — et s'est délibérément abstenue de la seconde :
//! « rien n'est écrit dans les fichiers ». La réserve n° 2 du fil forum 1343
//! en fixe le cadre : « l'écriture dans les fichiers de l'utilisateur doit
//! rester explicite et optionnelle, jamais automatique ».
//!
//! Le cœur tient cette règle (`tune_core::library::lyrics_pass`, tests
//! unitaires) ; ce fichier garde la **route**, c'est-à-dire le seul endroit
//! par lequel un utilisateur peut déclencher l'écriture :
//!
//! - sans `lyrics_write_files_enabled`, la route **refuse** (409) au lieu
//!   d'accepter un travail qui n'aura pas lieu ;
//! - avec le réglage, elle accepte (202) et le `.lrc` apparaît réellement à
//!   côté du fichier ;
//! - `GET /library/lyrics/status` dit l'état de ce consentement **sans** qu'il
//!   faille lancer la passe pour l'apprendre.
//!
//! ## Aucun réseau ici
//!
//! La passe d'écriture ne sort jamais : elle ne fait que rendre au disque ce
//! que `lyrics_cache` contient déjà. On sème le cache à la main, aucun appel à
//! `lrclib.net` n'est possible.
//!
//! Enregistré dans `server_contracts.rs` (`autotests = false` : un fichier non
//! déclaré n'est jamais compilé).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

fn make_app_with_state() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    appel(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn post(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    appel(app, Request::post(path).body(Body::empty()).unwrap()).await
}

fn reglage(state: &AppState, cle: &str, valeur: &str) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(cle, valeur)
        .expect("le réglage s'écrit");
}

/// Une piste avec un vrai fichier sur le disque et des paroles synchronisées
/// déjà en cache — l'état exact d'une bibliothèque après la passe LRCLIB.
fn piste_avec_paroles_en_cache(state: &AppState, dir: &tempfile::TempDir) -> String {
    let aid = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone())
        .create(&tune_core::db::models::Artist::new("Artiste".into()))
        .expect("insert artist");

    let chemin = dir.path().join("Morceau.flac");
    std::fs::write(&chemin, b"pas vraiment du son").unwrap();
    let chemin = chemin.to_str().unwrap().to_string();

    let mut t = tune_core::db::models::Track::new("Morceau".into());
    t.artist_id = Some(aid);
    t.duration_ms = 180_000;
    t.file_path = Some(chemin.clone());
    let tid = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insert track");

    tune_core::lyrics::store_cache_entry(
        &state.backend,
        tid,
        "Morceau",
        "Artiste",
        Some("[00:01.00] une ligne"),
        None,
    );
    chemin
}

/// Attend l'apparition d'un chemin, au plus `limite`. La passe répond 202 et
/// travaille en tâche de fond : sans attente, le test courserait le
/// planificateur.
async fn attendre_le_fichier(chemin: &std::path::Path, limite: std::time::Duration) -> bool {
    let debut = std::time::Instant::now();
    while debut.elapsed() < limite {
        if chemin.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    chemin.exists()
}

#[tokio::test]
async fn sans_consentement_la_route_refuse_et_ne_pose_aucun_fichier() {
    let (app, state) = make_app_with_state();
    let dir = tempfile::TempDir::new().unwrap();
    let audio = piste_avec_paroles_en_cache(&state, &dir);
    // `lyrics_write_files_enabled` volontairement NON positionné.

    let (status, body) = post(&app, "/api/v1/library/lyrics/write").await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "un refus franc, pas un 202 qui ne fera rien : {body}"
    );
    assert_eq!(body["reason"], "write_disabled");
    assert_eq!(body["setting"], "lyrics_write_files_enabled");

    // Le contre-témoin : rien n'est apparu à côté du fichier audio.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        tune_core::metadata::lyrics::find_sidecar_lrc(&audio).is_none(),
        "aucun `.lrc` ne doit exister apres un refus"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "le dossier de musique ne contient toujours que le fichier audio"
    );
}

#[tokio::test]
async fn avec_consentement_la_route_accepte_et_le_lrc_apparait() {
    let (app, state) = make_app_with_state();
    let dir = tempfile::TempDir::new().unwrap();
    let audio = piste_avec_paroles_en_cache(&state, &dir);
    reglage(&state, "lyrics_write_files_enabled", "true");

    let (status, body) = post(&app, "/api/v1/library/lyrics/write").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body["target"], "sidecar",
        "la cible par defaut n'ouvre pas le fichier audio"
    );

    let voisin = dir.path().join("Morceau.lrc");
    assert!(
        attendre_le_fichier(&voisin, std::time::Duration::from_secs(10)).await,
        "le `.lrc` voisin doit finir par apparaitre"
    );
    assert_eq!(
        tune_core::metadata::lyrics::find_sidecar_lrc(&audio).as_deref(),
        Some("[00:01.00] une ligne\n"),
        "et porter les paroles que Tune connaissait"
    );
}

#[tokio::test]
async fn l_indicateur_dit_l_etat_du_consentement_sans_rien_ecrire() {
    let (app, state) = make_app_with_state();
    let dir = tempfile::TempDir::new().unwrap();
    let audio = piste_avec_paroles_en_cache(&state, &dir);

    let (status, body) = get(&app, "/api/v1/library/lyrics/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["write_enabled"], false,
        "une interface doit pouvoir proposer d'activer le reglage : {body}"
    );
    assert_eq!(body["write_target"], "sidecar");
    assert_eq!(body["write_result"], Value::Null, "aucun run n'a eu lieu");

    reglage(&state, "lyrics_write_files_enabled", "true");
    reglage(&state, "lyrics_write_target", "tag");
    let (_, body) = get(&app, "/api/v1/library/lyrics/status").await;
    assert_eq!(body["write_enabled"], true);
    assert_eq!(body["write_target"], "tag");

    // L'indicateur reste une lecture : le consulter n'écrit rien.
    assert!(tune_core::metadata::lyrics::find_sidecar_lrc(&audio).is_none());
}
