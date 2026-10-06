//! #4805 (idée 4) — la passe par empreinte AcoustID, par la route montée.
//!
//! **Hermétique : aucun appel réel à AcoustID ni à MusicBrainz.** Une doublure
//! locale (127.0.0.1, port éphémère) sert les réponses AcoustID enregistrées
//! de `tune-core/tests/fixtures/acoustid/` (fabriquées, sans donnée réelle) et
//! les releases MusicBrainz ; un faux `fpcalc` (script shell) rend une
//! empreinte tirée du nom du fichier. Les limiteurs partagés restent en
//! place : la passe tourne à son vrai débit.
//!
//! Témoins :
//! 1. un album dont les pistes votent pour une release reçoit cette release et
//!    les enregistrements des pistes ; un album sans majorité n'est PAS
//!    touché ; un album jamais tenté par MusicBrainz n'est pas sélectionné ;
//! 2. AcoustID est interrogé à 3 requêtes/s au plus, avec la clé du RÉGLAGE ;
//! 3. sans `fpcalc`, la route refuse en 409 `fpcalc_absent` et l'écran Santé
//!    (`/system/background-tasks`) dit pourquoi ;
//! 4. pendant la lecture, la passe n'interroge rien ; elle part à l'arrêt.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

const PROPRE: &str = include_str!("../../tune-core/tests/fixtures/acoustid/propre.json");
const AMBIGU: &str = include_str!("../../tune-core/tests/fixtures/acoustid/ambigu.json");
const DUREE: &str = include_str!("../../tune-core/tests/fixtures/acoustid/duree.json");

/// Les témoins partagent des réglages globaux (bases remplacées, `fpcalc`) :
/// ils passent l'un après l'autre.
static SERIE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct Releve {
    /// Instant de chaque requête AcoustID, avec sa clé `client` et son `meta`.
    acoustid: Vec<(Instant, String, String)>,
    musicbrainz: Vec<String>,
}

type Partage = Arc<Mutex<Releve>>;

fn fixtures() -> Vec<Value> {
    [PROPRE, AMBIGU, DUREE]
        .iter()
        .map(|c| serde_json::from_str(c).unwrap())
        .collect()
}

/// La release MusicBrainz attendue d'un album du banc, ses pistes dans
/// l'ordre local, chacune avec l'enregistrement de la vérité.
fn release_mb(album: &Value) -> Option<(String, Value)> {
    let id = album["attendu"]["release_id"].as_str()?.to_string();
    let pistes: Vec<Value> = album["pistes"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let tid = p["track_id"].as_i64().unwrap().to_string();
            json!({
                "position": i + 1,
                "title": p["titre"],
                "recording": { "id": album["verite"][&tid] },
            })
        })
        .collect();
    Some((
        id.clone(),
        json!({
            "id": id,
            "title": album["album"]["titre"],
            "artist-credit": [{ "name": album["album"]["artiste"], "joinphrase": "" }],
            "label-info": [],
            "media": [{ "position": 1, "tracks": pistes }],
        }),
    ))
}

async fn doublure() -> Partage {
    let releve: Partage = Arc::default();
    let mut par_empreinte: HashMap<String, Value> = HashMap::new();
    let mut releases: HashMap<String, Value> = HashMap::new();
    for album in fixtures() {
        for p in album["pistes"].as_array().unwrap() {
            par_empreinte.insert(
                p["empreinte"].as_str().unwrap().to_string(),
                p["reponse"].clone(),
            );
        }
        if let Some((id, r)) = release_mb(&album) {
            releases.insert(id, r);
        }
    }
    let (r1, r2) = (releve.clone(), releve.clone());
    let app = axum::Router::new()
        .route(
            "/acoustid/lookup",
            axum::routing::post(move |axum::Form(f): axum::Form<HashMap<String, String>>| {
                let r = r1.clone();
                let reponse = f
                    .get("fingerprint")
                    .and_then(|e| par_empreinte.get(e))
                    .cloned()
                    .unwrap_or_else(|| json!({"status": "ok", "results": []}));
                async move {
                    r.lock().unwrap().acoustid.push((
                        Instant::now(),
                        f.get("client").cloned().unwrap_or_default(),
                        f.get("meta").cloned().unwrap_or_default(),
                    ));
                    axum::Json(reponse)
                }
            }),
        )
        .route(
            "/mb/release/{id}",
            axum::routing::get(move |Path(id): Path<String>| {
                let r = r2.clone();
                let corps = releases.get(&id).cloned();
                async move {
                    r.lock().unwrap().musicbrainz.push(id);
                    match corps {
                        Some(v) => axum::Json(v).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    tune_core::metadata::acoustid_picard::remplacer_la_base_acoustid(Some(format!(
        "http://{adresse}/acoustid"
    )));
    tune_core::metadata::musicbrainz_release::remplacer_la_base_musicbrainz(Some(format!(
        "http://{adresse}/mb"
    )));
    releve
}

/// Un faux `fpcalc` : `<track_id>-<durée>.flac` → empreinte `AQAD-fixture-<id>`.
fn faux_fpcalc(dossier: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let chemin = dossier.join("fpcalc");
    std::fs::write(
        &chemin,
        "#!/bin/sh\n\
         if [ \"$1\" = \"-version\" ]; then echo 'fpcalc version 1.5.1'; exit 0; fi\n\
         f=$(basename \"$2\" .flac); id=${f%%-*}; d=${f#*-}\n\
         printf '{\"duration\": %s.0, \"fingerprint\": \"AQAD-fixture-%s\"}\\n' \"$d\" \"$id\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o755)).unwrap();
    chemin
}

async fn etat_premium() -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("acoustid_api_key", "cle-de-test-du-reglage")
        .unwrap();
    state
}

/// Pose les albums du banc : `identification_tentee_le` posée (MusicBrainz a
/// répondu sans pressage), aucun MBID, une piste par fixture.
fn poser_les_albums(state: &tune_server::state::AppState, dossier: &std::path::Path) {
    for (rang, album) in fixtures().iter().enumerate() {
        let album_id = rang as i64 + 1;
        let titre = album["album"]["titre"].as_str().unwrap().to_string();
        state
            .backend
            .execute(
                "INSERT INTO albums (id, title, source, identification_tentee_le) \
                 VALUES (?, ?, 'local', '2026-10-01T00:00:00Z')",
                &[&album_id as &dyn ToSqlValue, &titre as &dyn ToSqlValue],
            )
            .unwrap();
        for (i, p) in album["pistes"].as_array().unwrap().iter().enumerate() {
            let tid = p["track_id"].as_i64().unwrap();
            let duree = p["duree_s"].as_i64().unwrap();
            let chemin = dossier
                .join(format!("{tid}-{duree}.flac"))
                .to_string_lossy()
                .into_owned();
            let titre_piste = p["titre"].as_str().unwrap().to_string();
            let numero = i as i64 + 1;
            let duree_ms = duree * 1000;
            state
                .backend
                .execute(
                    "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number, \
                     file_path, duration_ms) VALUES (?, ?, ?, 'local', ?, 1, ?, ?)",
                    &[
                        &tid as &dyn ToSqlValue,
                        &titre_piste as &dyn ToSqlValue,
                        &album_id as &dyn ToSqlValue,
                        &numero as &dyn ToSqlValue,
                        &chemin as &dyn ToSqlValue,
                        &duree_ms as &dyn ToSqlValue,
                    ],
                )
                .unwrap();
        }
    }
    // Un album jamais tenté par la recherche texte : hors de la passe.
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title, source) VALUES (9, 'Jamais tenté', 'local')",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number, \
             file_path, duration_ms) VALUES (901, 'X', 9, 'local', 1, 1, '/x/901-200.flac', 200000)",
            &[],
        )
        .unwrap();
}

async fn appeler(app: &axum::Router, methode: &str, path: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn attendre_la_fin(app: &axum::Router) -> Value {
    for _ in 0..240 {
        let (_, etat) = appeler(app, "GET", "/api/v1/library/identify-all/status").await;
        if etat["status"].as_str() != Some("running") {
            return etat;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("la passe n'a pas fini en 60 s");
}

fn release_de(state: &tune_server::state::AppState, album_id: i64) -> Option<String> {
    state
        .backend
        .query_one(
            "SELECT musicbrainz_release_id FROM albums WHERE id = ?",
            &[&album_id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_string()
}

fn enregistrement_de(state: &tune_server::state::AppState, track_id: i64) -> Option<String> {
    state
        .backend
        .query_one(
            "SELECT musicbrainz_recording_id FROM tracks WHERE id = ?",
            &[&track_id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_string()
}

/// Témoins 1 et 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_passe_ecrit_la_release_majoritaire_et_rien_d_autre() {
    let _serie = SERIE.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    tune_core::metadata::fingerprint::remplacer_fpcalc(Some(faux_fpcalc(dossier.path())));
    let releve = doublure().await;
    let state = etat_premium().await;
    poser_les_albums(&state, dossier.path());
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all?mode=acoustid").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    assert_eq!(
        corps["total"], 3,
        "l'album jamais tenté n'est pas candidat : {corps}"
    );
    let etat = attendre_la_fin(&app).await;
    assert_eq!(etat["status"], "done", "{etat}");
    assert_eq!(etat["mode"], "acoustid", "{etat}");
    assert_eq!(etat["identifies"], 2, "{etat}");
    assert_eq!(etat["sans_majorite"], 1, "{etat}");

    let banc = fixtures();
    // L'album propre (1) et l'album « durée » (3) reçoivent LEUR release.
    for (album_id, fixture) in [(1, &banc[0]), (3, &banc[2])] {
        assert_eq!(
            release_de(&state, album_id).as_deref(),
            fixture["attendu"]["release_id"].as_str(),
            "album {album_id}"
        );
        for (tid, rec) in fixture["attendu"]["enregistrements"].as_object().unwrap() {
            assert_eq!(
                enregistrement_de(&state, tid.parse().unwrap()).as_deref(),
                rec.as_str(),
                "piste {tid}"
            );
        }
    }
    // L'album ambigu (2) n'est pas touché, ni ses pistes.
    assert_eq!(release_de(&state, 2), None);
    for tid in [201, 202, 203] {
        assert_eq!(enregistrement_de(&state, tid), None, "piste {tid}");
    }
    // Ni l'album jamais tenté.
    assert_eq!(release_de(&state, 9), None);

    let r = releve.lock().unwrap();
    // Une requête AcoustID par piste des trois albums (4 + 3 + 2), avec la
    // clé du réglage et le meta de Picard.
    assert_eq!(r.acoustid.len(), 9);
    assert!(
        r.acoustid
            .iter()
            .all(|(_, cle, _)| cle == "cle-de-test-du-reglage")
    );
    assert!(
        r.acoustid
            .iter()
            .all(|(_, _, meta)| meta == "recordings releasegroups releases compress")
    );
    // 3 requêtes/s au plus : sur toute fenêtre de quatre requêtes
    // consécutives, au moins une seconde (à la tolérance d'horloge près).
    for w in r.acoustid.windows(4) {
        let ecart = w[3].0.duration_since(w[0].0);
        assert!(
            ecart >= Duration::from_millis(950),
            "4 requêtes AcoustID en {ecart:?} : plus de 3/s"
        );
    }
    // Une lecture MusicBrainz par album retenu, aucune pour l'ambigu.
    assert_eq!(r.musicbrainz.len(), 2, "{:?}", r.musicbrainz);
    drop(r);

    // Les empreintes sont en base : une reprise ne redécode rien.
    let n = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM tracks WHERE acoustid_fingerprint LIKE 'AQAD-fixture-%'",
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64();
    assert_eq!(n, Some(9));
    tune_core::metadata::fingerprint::remplacer_fpcalc(None);
}

/// Témoin 3 — sans `fpcalc`, refus franc et message dans Santé.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sans_fpcalc_la_passe_se_desactive_et_le_dit() {
    let _serie = SERIE.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    tune_core::metadata::fingerprint::remplacer_fpcalc(Some(dossier.path().join("absent")));
    let releve = doublure().await;
    let state = etat_premium().await;
    poser_les_albums(&state, dossier.path());
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all?mode=acoustid").await;
    assert_eq!(status, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["code"], "fpcalc_absent", "{corps}");

    let (status, sante) = appeler(&app, "GET", "/api/v1/system/background-tasks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sante["acoustid"]["available"], false, "{sante}");
    assert_eq!(sante["acoustid"]["reason"], "fpcalc_absent", "{sante}");
    assert!(
        sante["acoustid"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("fpcalc")),
        "{sante}"
    );
    assert!(releve.lock().unwrap().acoustid.is_empty());
    tune_core::metadata::fingerprint::remplacer_fpcalc(None);
}

/// Témoin 4 — hors lecture : rien ne part tant qu'une zone joue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_passe_attend_la_fin_de_la_lecture() {
    let _serie = SERIE.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    tune_core::metadata::fingerprint::remplacer_fpcalc(Some(faux_fpcalc(dossier.path())));
    let releve = doublure().await;
    let state = etat_premium().await;
    poser_les_albums(&state, dossier.path());
    let app = tune_server::routes::router(state.clone());

    tune_core::taches_de_fond::priorite::noter_etat_de_lecture(4805, "playing");
    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all?mode=acoustid").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        releve.lock().unwrap().acoustid.is_empty(),
        "une requête AcoustID est partie pendant la lecture"
    );
    let (_, etat) = appeler(&app, "GET", "/api/v1/library/identify-all/status").await;
    assert_eq!(etat["status"], "running", "{etat}");

    tune_core::taches_de_fond::priorite::noter_etat_de_lecture(4805, "stopped");
    let etat = attendre_la_fin(&app).await;
    assert_eq!(etat["status"], "done", "{etat}");
    assert_eq!(etat["identifies"], 2, "{etat}");
    tune_core::metadata::fingerprint::remplacer_fpcalc(None);
}
