//! Témoins de #7763 — les imports reportent les écoutes et les notes.
//!
//! Par le routeur réel (`/api/v1/system/import/...`), avec un vrai corps
//! multipart, comme l'écran d'import l'envoie :
//!
//! * Roon / Plex : les écoutes, la dernière écoute et la note de l'export vont
//!   sur les pistes DÉJÀ en bibliothèque ; le rapport les compte ;
//! * m3u : N écoutes par piste trouvée de la liste, une fois par piste ;
//! * l'aperçu (`?preview=true`) rend les mêmes chiffres et n'écrit RIEN ;
//! * aucune ligne n'entre dans `listen_history`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::DbBackend;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Une piste de la bibliothèque, sans artiste (l'historique la rapproche par
/// titre, interprète absent des deux côtés).
fn piste(db: &Arc<dyn DbBackend>, titre: &str, chemin: &str) -> i64 {
    let mut t = Track::new(titre.to_string());
    t.file_path = Some(chemin.to_string());
    TrackRepo::with_backend(db.clone()).create(&t).unwrap()
}

fn ecoute(db: &Arc<dyn DbBackend>, titre: &str, quand: &str) {
    db.execute(
        "INSERT INTO listen_history (title, source, listened_at) VALUES (?, 'local', ?)",
        &[&titre, &quand],
    )
    .unwrap();
}

fn compter(db: &Arc<dyn DbBackend>, sql: &str) -> i64 {
    db.query_one(sql, &[])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

fn note(db: &Arc<dyn DbBackend>, id: i64) -> Option<i64> {
    db.query_one("SELECT rating FROM tracks WHERE id = ?", &[&id])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
}

fn ecoutes(db: &Arc<dyn DbBackend>, id: i64) -> (i64, Option<String>) {
    HistoryRepo::with_backend(db.clone())
        .plays_for_tracks(&[id])
        .unwrap()
        .remove(&id)
        .unwrap()
}

fn multipart(nom: &str, contenu: &str) -> (String, Vec<u8>) {
    let frontiere = "----tune7763Ecoutes";
    let corps = format!(
        "--{frontiere}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{nom}\"\r\n\
         Content-Type: application/octet-stream\r\n\
         \r\n\
         {contenu}\r\n\
         --{frontiere}--\r\n"
    );
    (
        format!("multipart/form-data; boundary={frontiere}"),
        corps.into_bytes(),
    )
}

async fn appeler(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
    let reponse = crate::routes::router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    (
        statut,
        serde_json::from_str(&texte).unwrap_or(json!({ "_brut": texte })),
    )
}

async fn televerser(
    state: &AppState,
    chemin: &str,
    nom: &str,
    contenu: &str,
) -> (StatusCode, Value) {
    let (type_de_contenu, corps) = multipart(nom, contenu);
    appeler(
        state,
        Request::post(chemin)
            .header(header::CONTENT_TYPE, type_de_contenu)
            .body(Body::from(corps))
            .unwrap(),
    )
    .await
}

/// L'import réel travaille en tâche détachée : attendre son rapport.
async fn rapport_de_la_tache(state: &AppState, task_id: &str) -> Value {
    let fin = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, v) = appeler(
            state,
            Request::get(format!("/api/v1/system/import/status/{task_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        if v["status"] != "running" && v["status"] != "unknown" {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < fin,
            "la tâche d'import ne s'est pas terminée : {v}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Un export Roon : « Walking » est déjà là (2 écoutes d'historique, sans
/// note), « Inconnue » ne l'est pas.
const CSV_ROON: &str = "Title,File Path,Play Count,Rating,Last Played\r\n\
     Walking,/music/walking.flac,5,4,2026-11-01 08:00:00\r\n\
     Inconnue,/music/inconnue.flac,9,5,2026-11-01 08:00:00\r\n";

fn bibliotheque_roon(state: &AppState) -> i64 {
    let id = piste(&state.backend, "Walking", "/music/walking.flac");
    ecoute(&state.backend, "Walking", "2026-10-01T10:00:00Z");
    ecoute(&state.backend, "Walking", "2026-10-02T10:00:00Z");
    id
}

/// L'aperçu chiffre les reports et n'écrit RIEN : ni écoute importée, ni
/// note, ni piste, ni historique.
#[tokio::test]
async fn l_apercu_roon_chiffre_les_reports_sans_rien_ecrire_7763() {
    let state = etat();
    let id = bibliotheque_roon(&state);
    let db = &state.backend;
    let pistes_avant = compter(db, "SELECT COUNT(*) FROM tracks");

    let (statut, rapport) = televerser(
        &state,
        "/api/v1/system/import/roon?preview=true",
        "roon.csv",
        CSV_ROON,
    )
    .await;

    assert_eq!(statut, StatusCode::OK, "{rapport}");
    assert_eq!(rapport["matched"], 1);
    assert_eq!(rapport["unmatched"], 1);
    assert_eq!(
        rapport["play_counts_updated"], 1,
        "l'aperçu doit annoncer les écoutes à reporter : {rapport}"
    );
    assert_eq!(rapport["plays_added"], 5);
    assert_eq!(rapport["ratings_updated"], 1);
    assert_eq!(rapport["history_entries_added"], 0);

    assert_eq!(
        compter(db, "SELECT COUNT(*) FROM imported_plays"),
        0,
        "l'aperçu ne doit écrire aucune écoute importée"
    );
    assert_eq!(note(db, id), None, "l'aperçu ne doit pas poser de note");
    assert_eq!(compter(db, "SELECT COUNT(*) FROM tracks"), pistes_avant);
    assert_eq!(compter(db, "SELECT COUNT(*) FROM listen_history"), 2);
    assert_eq!(ecoutes(db, id).0, 2);
}

/// L'import réel ajoute les écoutes aux existantes, pose la note et la date
/// plus récente, sans une ligne d'historique ; la piste créée par l'import ne
/// reçoit rien.
#[tokio::test]
async fn l_import_roon_ajoute_les_ecoutes_et_la_note_aux_pistes_rapprochees_7763() {
    let state = etat();
    let id = bibliotheque_roon(&state);
    let db = &state.backend;

    let (statut, reponse) =
        televerser(&state, "/api/v1/system/import/roon", "roon.csv", CSV_ROON).await;
    assert_eq!(statut, StatusCode::ACCEPTED, "{reponse}");
    let rapport = rapport_de_la_tache(&state, reponse["task_id"].as_str().unwrap()).await;

    assert_eq!(rapport["status"], "completed", "{rapport}");
    assert_eq!(rapport["play_counts_updated"], 1, "{rapport}");
    assert_eq!(rapport["plays_added"], 5);
    assert_eq!(rapport["ratings_updated"], 1);
    assert_eq!(
        ecoutes(db, id),
        (7, Some("2026-11-01T08:00:00Z".to_string())),
        "2 écoutes d'historique + 5 importées, et la date importée plus récente"
    );
    assert_eq!(
        note(db, id),
        Some(4),
        "la note de l'export doit être reportée"
    );
    assert_eq!(
        compter(db, "SELECT COUNT(*) FROM listen_history"),
        2,
        "aucune fausse entrée d'historique"
    );
    let nouvelle = TrackRepo::with_backend(db.clone())
        .get_by_path("/music/inconnue.flac")
        .unwrap()
        .and_then(|t| t.id)
        .expect("l'import crée toujours les pistes absentes");
    assert_eq!(
        ecoutes(db, nouvelle).0,
        0,
        "une piste créée par l'import n'est pas une piste rapprochée"
    );
}

/// Plex : une dernière écoute importée plus ANCIENNE que l'historique ne
/// change pas la dernière écoute ; les écoutes s'ajoutent quand même.
#[tokio::test]
async fn une_date_plex_plus_ancienne_laisse_la_derniere_ecoute_7763() {
    let state = etat();
    let db = &state.backend;
    let id = piste(db, "Time", "/music/time.flac");
    ecoute(db, "Time", "2026-10-02T10:00:00Z");
    // 1700000000 : 14/11/2023, plus ancien que l'historique.
    let xml = r#"<MediaContainer><Track title="Time" viewCount="3" lastViewedAt="1700000000" /></MediaContainer>"#;

    let (statut, reponse) = televerser(&state, "/api/v1/system/import/plex", "plex.xml", xml).await;
    assert_eq!(statut, StatusCode::ACCEPTED, "{reponse}");
    let rapport = rapport_de_la_tache(&state, reponse["task_id"].as_str().unwrap()).await;

    assert_eq!(rapport["plays_added"], 3, "{rapport}");
    assert_eq!(
        ecoutes(db, id),
        (4, Some("2026-10-02T10:00:00Z".to_string())),
        "une date importée plus ancienne ne doit pas remplacer la dernière écoute"
    );
}

/// Une liste m3u : deux pistes trouvées (dont une citée deux fois), une
/// absente, une ligne de service.
const M3U: &str = "#EXTM3U\r\n\
     /music/a.flac\r\n\
     /music/a.flac\r\n\
     /music/b.flac\r\n\
     /music/absente-7763-zzz.flac\r\n\
     http://exemple.invalid/flux.mp3\r\n";

#[tokio::test]
async fn l_import_m3u_ajoute_n_ecoutes_a_chaque_piste_trouvee_7763() {
    let state = etat();
    let db = &state.backend;
    let a = piste(db, "Alpha", "/music/a.flac");
    let b = piste(db, "Beta", "/music/b.flac");
    let c = piste(db, "Gamma", "/music/c.flac");
    ecoute(db, "Alpha", "2026-10-02T10:00:00Z");

    // L'aperçu : les chiffres, rien d'écrit.
    let (statut, apercu) = televerser(
        &state,
        "/api/v1/system/import/m3u-plays?plays=3&preview=true",
        "liste.m3u",
        M3U,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["preview"], true);
    assert_eq!(apercu["total_rows"], 5);
    assert_eq!(apercu["matched"], 3);
    assert_eq!(apercu["unmatched"], 2);
    assert_eq!(
        apercu["duplicates_skipped"], 1,
        "une piste citée deux fois ne reçoit ses écoutes qu'une fois : {apercu}"
    );
    assert_eq!(apercu["play_counts_updated"], 2);
    assert_eq!(apercu["plays_added"], 6);
    assert_eq!(
        compter(db, "SELECT COUNT(*) FROM imported_plays"),
        0,
        "l'aperçu m3u ne doit rien écrire"
    );

    // L'import réel.
    let (statut, rapport) = televerser(
        &state,
        "/api/v1/system/import/m3u-plays?plays=3",
        "liste.m3u",
        M3U,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rapport}");
    assert_eq!(rapport["play_counts_updated"], 2);
    assert_eq!(rapport["plays_added"], 6);
    assert_eq!(
        rapport["not_found_paths"],
        json!([
            "/music/absente-7763-zzz.flac",
            "http://exemple.invalid/flux.mp3"
        ])
    );
    assert_eq!(
        ecoutes(db, a),
        (4, Some("2026-10-02T10:00:00Z".to_string())),
        "1 écoute d'historique + 3, une seule fois malgré deux lignes ; la \
         liste ne porte pas de date, la dernière écoute ne bouge pas"
    );
    assert_eq!(ecoutes(db, b), (3, None));
    assert_eq!(
        ecoutes(db, c).0,
        0,
        "une piste hors de la liste ne reçoit rien"
    );
    assert_eq!(compter(db, "SELECT COUNT(*) FROM listen_history"), 1);
    assert_eq!(
        compter(db, "SELECT COUNT(*) FROM playlists"),
        0,
        "l'import d'écoutes ne crée pas de liste de lecture"
    );
}

#[tokio::test]
async fn l_import_m3u_refuse_un_nombre_d_ecoutes_hors_bornes_7763() {
    let state = etat();
    piste(&state.backend, "Alpha", "/music/a.flac");
    for requete in ["", "?plays=0", "?plays=1001", "?plays=-2"] {
        let (statut, v) = televerser(
            &state,
            &format!("/api/v1/system/import/m3u-plays{requete}"),
            "liste.m3u",
            M3U,
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{requete} : {v}");
        assert_eq!(v["error"], "plays_hors_bornes", "{requete}");
    }
    assert_eq!(
        compter(&state.backend, "SELECT COUNT(*) FROM imported_plays"),
        0
    );
}
