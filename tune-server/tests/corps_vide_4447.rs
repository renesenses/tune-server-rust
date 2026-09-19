//! #4447 — un corps JSON optionnel doit l'être AUSSI quand le client annonce
//! `application/json` sans rien envoyer.
//!
//! Mesuré sur le .18 en v0.9.155, écran Studio → Métadonnées → Manquants :
//!
//! ```text
//! POST /api/v1/library/enrich-all  sans Content-Type                     → 202
//! POST /api/v1/library/enrich-all  Content-Type: application/json, vide  → 400
//!      Failed to parse the request body as JSON: EOF while parsing a value…
//! ```
//!
//! `Option<Json<T>>` ne rend `None` QUE si l'en-tête est absent : présent avec
//! un corps vide, l'extracteur désérialise, échoue, et le rejet remonte. Trois
//! routes que le client web appelle sans charge utile portaient ce défaut —
//! `/library/enrich-all` (bouton « Retrouver genres et années »),
//! `/metadata/auto-fix` et `/zones/{id}/queue/clear` (`clearQueue`).
//!
//! **Le cas du milieu est le témoin** : c'est lui qui rougit si l'on remet
//! `Option<Json<…>>`. Les deux autres verrouillent l'absence de régression.
//!
//! Hermétique : aucun appel réseau. Les cas acceptés rendent leur 202 avant
//! toute requête sortante, et la bibliothèque témoin n'offre aucun candidat.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// Les TROIS façons d'appeler une route à corps optionnel.
enum Corps {
    /// Aucun en-tête `Content-Type`, aucun octet : le cas qui marchait déjà.
    SansEntete,
    /// `Content-Type: application/json` et zéro octet : LE cas de l'issue.
    EnteteJsonEtVide,
    /// Un corps JSON valide : comportement à ne pas bouger.
    Json(&'static str),
}

async fn poster(app: &axum::Router, chemin: &str, corps: Corps) -> (StatusCode, Value) {
    let req = match corps {
        Corps::SansEntete => Request::post(chemin).body(Body::empty()).unwrap(),
        Corps::EnteteJsonEtVide => Request::post(chemin)
            .header("Content-Type", "application/json")
            .body(Body::empty())
            .unwrap(),
        Corps::Json(j) => Request::post(chemin)
            .header("Content-Type", "application/json")
            .body(Body::from(j))
            .unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (status, json)
}

/// Bibliothèque à une racine dont AUCUNE piste n'est candidate à
/// l'enrichissement : chaque champ que la sélection teste est rempli. La passe
/// acceptée n'a donc rien à demander à MusicBrainz.
fn peupler(state: &tune_server::state::AppState) {
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", r#"["/music-4447"]"#)
        .unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name, musicbrainz_id) VALUES (1, 'Miles Davis', 'mbid-1'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES (1, 'Kind of Blue', 1, 'c1'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source, \
                                 musicbrainz_recording_id, genre, year, label, composer) \
               VALUES (10, 'So What', 1, 1, '/music-4447/jazz/Kind of Blue/01.flac', 'local', \
                       'rec-10', 'Jazz', 1959, 'Columbia', 'Miles Davis');",
        )
        .unwrap();
}

/// Le message exact que le bandeau rouge affichait. Aucune des trois routes ne
/// doit plus le produire, sous aucune des trois formes d'appel.
fn pas_d_eof(status: StatusCode, body: &Value, ou: &str) {
    let texte = body.to_string();
    assert!(
        !texte.contains("EOF while parsing"),
        "{ou} : le rejet JSON d'un corps vide est remonté au client — {texte}"
    );
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "{ou} : un corps vide n'est pas une requête invalide — {texte}"
    );
}

// ---------------------------------------------------------------------------
// POST /library/enrich-all — la route de « Retrouver genres et années »
// ---------------------------------------------------------------------------

const ENRICH_ALL: &str = "/api/v1/library/enrich-all";

/// Cas 1/3 — sans `Content-Type` : 202, passe complète. C'est le contrat
/// historique, celui qui répondait déjà juste.
#[tokio::test]
async fn enrich_all_sans_entete_reste_accepte() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = poster(&app, ENRICH_ALL, Corps::SansEntete).await;

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body["directory"].is_null(), "sans path : passe complète");
}

/// Cas 2/3 — **LE témoin de #4447** : `Content-Type: application/json` et zéro
/// octet. Répondait 400 « EOF while parsing a value at line 1 column 0 ». Doit
/// répondre exactement comme le cas 1 : 202, portée nulle.
#[tokio::test]
async fn enrich_all_entete_json_et_corps_vide_est_accepte() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = poster(&app, ENRICH_ALL, Corps::EnteteJsonEtVide).await;

    pas_d_eof(status, &body, "POST /library/enrich-all");
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "un POST annoncé JSON sans corps vaut un POST sans corps — {body}"
    );
    assert!(
        body["directory"].is_null(),
        "corps vide = portée nulle = bibliothèque entière, comme sans en-tête"
    );
}

/// Cas 3/3 — corps valide : la portée se calcule toujours, à la ligne près.
#[tokio::test]
async fn enrich_all_corps_valide_inchange() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = poster(
        &app,
        ENRICH_ALL,
        Corps::Json(r#"{"path":"/music-4447/jazz"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["directory"].as_str(), Some("/music-4447/jazz"));
}

/// Et le refus franc d'un chemin invalide n'est pas amolli : le nouvel
/// extracteur ne doit pas transformer un mauvais `path` en passe complète —
/// ce serait enrichir exactement ce que l'utilisateur voulait épargner.
#[tokio::test]
async fn enrich_all_chemin_hors_racines_toujours_refuse() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = poster(&app, ENRICH_ALL, Corps::Json(r#"{"path":"/ailleurs"}"#)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"].as_str(), Some("path_outside_music_dirs"));
}

/// Un corps PRÉSENT mais illisible reste une erreur du client : on ne le fait
/// pas retomber en silence sur la passe complète.
#[tokio::test]
async fn enrich_all_corps_illisible_reste_refuse() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = poster(&app, ENRICH_ALL, Corps::Json("{pas du json")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ---------------------------------------------------------------------------
// Les deux autres routes que le client web appelle sans charge utile
// ---------------------------------------------------------------------------

/// `POST /metadata/auto-fix` (`startAutoFix`, même écran) : le POST annoncé
/// JSON sans corps ne doit plus être rejeté à la porte. Un seul appel ici —
/// l'état du balayage est partagé par le processus, un second appel répondrait
/// 409 pour une raison qui n'a rien à voir avec l'en-tête.
#[tokio::test]
async fn auto_fix_entete_json_et_corps_vide_nest_pas_un_400() {
    let (app, state) = app_et_etat();
    peupler(&state);

    let (status, body) = poster(&app, "/api/v1/metadata/auto-fix", Corps::EnteteJsonEtVide).await;

    pas_d_eof(status, &body, "POST /metadata/auto-fix");
    assert!(
        body["status"].is_string() || body["error"].is_string(),
        "la route a rendu SA réponse, pas un rejet d'extracteur — {body}"
    );
}

/// `POST /zones/{id}/queue/clear` (`clearQueue`) : même défaut, même garde.
#[tokio::test]
async fn queue_clear_entete_json_et_corps_vide_nest_pas_un_400() {
    let (app_sans, etat_sans) = app_et_etat();
    peupler(&etat_sans);
    let (app_vide, etat_vide) = app_et_etat();
    peupler(&etat_vide);

    let (sans, body_sans) =
        poster(&app_sans, "/api/v1/zones/1/queue/clear", Corps::SansEntete).await;
    let (vide, body_vide) = poster(
        &app_vide,
        "/api/v1/zones/1/queue/clear",
        Corps::EnteteJsonEtVide,
    )
    .await;

    pas_d_eof(vide, &body_vide, "POST /zones/1/queue/clear");
    assert_eq!(
        vide, sans,
        "annoncer JSON sans corps ne doit rien changer — {body_vide} vs {body_sans}"
    );
}
