//! #4805 D — le pilote `identify-all` choisit sûrement, ou s'abstient.
//!
//! **Hermétique : aucun appel réel à MusicBrainz.** Même doublure locale que
//! `labels_par_le_pilote_4836.rs` (127.0.0.1, port éphémère), qui compte
//! chaque requête. Le limiteur partagé reste en place.
//!
//! Trois témoins, par la VRAIE route `POST /library/identify-all` :
//! 1. un album dont les balises portent déjà le MBID de release est identifié
//!    par ce MBID — une lecture, **aucune** recherche texte ;
//! 2. un album dont la recherche rend deux albums à égalité est **ambigu** :
//!    rien n'est écrit, il est compté, et il est marqué « déjà tenté » ;
//! 3. un album sûr est identifié comme avant, par la recherche.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

const MBID_BALISE: &str = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";
/// `Buddha-Bar: Ocean`, l'édition que l'utilisateur choisit.
const MBID_OCEAN: &str = "0c2a6c47-6f5e-4a54-9d3e-2d3f0b9a1c11";

/// Les témoins partagent la base MusicBrainz remplacée (globale).
static SERIE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type Compteur = Arc<Mutex<HashMap<String, usize>>>;

fn compter(c: &Compteur, cle: String) {
    *c.lock().unwrap().entry(cle).or_insert(0) += 1;
}

fn nb_prefixe(c: &Compteur, prefixe: &str) -> usize {
    c.lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with(prefixe))
        .map(|(_, n)| n)
        .sum()
}

fn hit(id: &str, titre: &str, artiste: &str, pistes: u64) -> Value {
    json!({
        "id": id,
        "score": 100,
        "title": titre,
        "status": "Official",
        "track-count": pistes,
        "artist-credit": [{ "name": artiste, "joinphrase": "" }],
        "release-group": { "id": format!("rg-{id}") },
    })
}

fn reponse_recherche(requete: &str) -> Value {
    if requete.contains("Buddha-Bar") {
        // Deux volumes différents, à 100 tous les deux.
        json!({ "releases": [
            hit("rel-xxiv", "Buddha‐Bar XXIV", "Various Artists", 2),
            hit(MBID_OCEAN, "Buddha-Bar: Ocean", "Various Artists", 2),
        ]})
    } else if requete.contains("Kind of Blue") {
        json!({ "releases": [hit("rel-kob", "Kind of Blue", "Miles Davis", 2)] })
    } else {
        json!({ "releases": [] })
    }
}

fn reponse_release(id: &str) -> Option<Value> {
    let (titre, groupe) = match id {
        MBID_BALISE => ("Nefertiti", "rg-balise"),
        "rel-kob" => ("Kind of Blue", "rg-rel-kob"),
        MBID_OCEAN => ("Buddha-Bar: Ocean", "rg-ocean"),
        _ => return None,
    };
    Some(json!({
        "id": id,
        "title": titre,
        "status": "Official",
        "release-group": { "id": groupe },
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }],
        "media": [{ "position": 1, "tracks": [
            { "position": 1, "title": "Un", "recording": { "id": format!("rec-{id}-1") } },
            { "position": 2, "title": "Deux", "recording": { "id": format!("rec-{id}-2") } }
        ]}],
    }))
}

async fn doublure() -> Compteur {
    let compteur: Compteur = Arc::default();
    let c1 = compteur.clone();
    let c2 = compteur.clone();
    let app = axum::Router::new()
        .route(
            "/release",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| {
                let c = c1.clone();
                async move {
                    let requete = q.get("query").cloned().unwrap_or_default();
                    compter(&c, format!("recherche:{requete}"));
                    axum::Json(reponse_recherche(&requete))
                }
            }),
        )
        .route(
            "/release/{id}",
            axum::routing::get(move |Path(id): Path<String>| {
                let c = c2.clone();
                async move {
                    compter(&c, format!("release/{id}"));
                    match reponse_release(&id) {
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
    tune_core::metadata::musicbrainz_release::remplacer_la_base_musicbrainz(Some(format!(
        "http://{adresse}"
    )));
    compteur
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

fn colonne(state: &tune_server::state::AppState, sql: &str, id: i64) -> Option<String> {
    state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .unwrap()
        .and_then(|row| row[0].as_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_pilote_part_des_balises_et_s_abstient_quand_c_est_ambigu() {
    let _serie = SERIE.lock().await;
    let compteur = doublure().await;
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    let sql = [
        // 1 — les deux pistes portent le MBID de release dans leurs balises
        //     (`track_metadata.mb_release_id`, posé par le scan). Le titre de
        //     l'album ne ressemble à rien : seule la balise peut l'identifier.
        "INSERT INTO albums (id, title, source) VALUES (1, 'Dossier 2003', 'local')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (10, 'Un', 1, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (11, 'Deux', 1, 'local', 2, 1)",
        // 2 — deux albums à égalité : ambigu.
        "INSERT INTO albums (id, title, source) VALUES (2, 'Buddha-Bar', 'local')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (20, 'Un', 2, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (21, 'Deux', 2, 'local', 2, 1)",
        // 3 — un seul candidat : identifié par la recherche, comme avant.
        "INSERT INTO albums (id, title, source) VALUES (3, 'Kind of Blue', 'local')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (30, 'Un', 3, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (31, 'Deux', 3, 'local', 2, 1)",
    ];
    for requete in sql {
        state.backend.execute(requete, &[]).unwrap();
    }
    for piste in [10i64, 11] {
        state
            .backend
            .execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'mb_release_id', ?)",
                &[&piste as &dyn ToSqlValue, &MBID_BALISE as &dyn ToSqlValue],
            )
            .unwrap();
    }
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    let etat = attendre_la_fin(&app).await;
    assert_eq!(etat["status"], "done", "{etat}");
    assert_eq!(etat["traites"], 3, "{etat}");
    assert_eq!(etat["identifies"], 2, "{etat}");
    assert_eq!(etat["ambigus"], 1, "{etat}");
    assert_eq!(etat["sans_correspondance"], 1, "{etat}");
    assert_eq!(etat["sources"]["balise_release"], 1, "{etat}");
    assert_eq!(etat["sources"]["recherche"], 1, "{etat}");

    let release = "SELECT musicbrainz_release_id FROM albums WHERE id = ?";
    let groupe = "SELECT musicbrainz_release_group_id FROM albums WHERE id = ?";
    let tente = "SELECT identification_tentee_le FROM albums WHERE id = ?";
    // Témoin 1 : le MBID des balises, son groupe, ses enregistrements.
    assert_eq!(colonne(&state, release, 1).as_deref(), Some(MBID_BALISE));
    assert_eq!(colonne(&state, groupe, 1).as_deref(), Some("rg-balise"));
    assert_eq!(
        colonne(
            &state,
            "SELECT musicbrainz_recording_id FROM tracks WHERE id = ?",
            11
        )
        .as_deref(),
        Some(format!("rec-{MBID_BALISE}-2").as_str())
    );
    // Témoin 2 : rien d'écrit, mais l'album est marqué tenté.
    assert_eq!(
        colonne(&state, release, 2),
        None,
        "un album ambigu ne reçoit rien"
    );
    assert!(
        colonne(&state, tente, 2).is_some(),
        "un album ambigu passe derrière le neuf"
    );
    // Témoin 3.
    assert_eq!(colonne(&state, release, 3).as_deref(), Some("rel-kob"));

    // Coût : l'album balisé n'a fait AUCUNE recherche (deux recherches pour
    // trois albums) ; l'ambigu n'a lu aucun détail.
    assert_eq!(
        nb_prefixe(&compteur, "recherche:"),
        2,
        "{:?}",
        compteur.lock().unwrap()
    );
    assert_eq!(
        nb_prefixe(&compteur, "recherche:release:\"Dossier 2003\""),
        0
    );
    assert_eq!(nb_prefixe(&compteur, &format!("release/{MBID_BALISE}")), 1);
    assert_eq!(
        nb_prefixe(&compteur, "release/rel-"),
        1,
        "seul Kind of Blue lit son détail"
    );
}

/// Décision de Bertrand (05/10/2026) : le bouton « Ré-identifier » suit la
/// même règle. Ambigu : il le DIT, n'écrit rien et rend les candidats ;
/// `?release_id=` impose l'édition choisie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_bouton_dit_l_ambiguite_et_laisse_choisir_l_edition() {
    let _serie = SERIE.lock().await;
    let compteur = doublure().await;
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    for requete in [
        "INSERT INTO albums (id, title, source) VALUES (2, 'Buddha-Bar', 'local')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (20, 'Un', 2, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (21, 'Deux', 2, 'local', 2, 1)",
        // Balisé : le bouton part lui aussi des balises.
        "INSERT INTO albums (id, title, source) VALUES (1, 'Dossier 2003', 'local')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (10, 'Un', 1, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (11, 'Deux', 1, 'local', 2, 1)",
    ] {
        state.backend.execute(requete, &[]).unwrap();
    }
    for piste in [10i64, 11] {
        state
            .backend
            .execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'mb_release_id', ?)",
                &[&piste as &dyn ToSqlValue, &MBID_BALISE as &dyn ToSqlValue],
            )
            .unwrap();
    }
    let app = tune_server::routes::router(state.clone());
    let release = "SELECT musicbrainz_release_id FROM albums WHERE id = ?";

    // 1. Ambigu : dit, rien d'écrit, candidats rendus.
    let (status, corps) = appeler(&app, "POST", "/api/v1/library/albums/2/reidentify").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["verdict"], "ambiguous", "{corps}");
    assert_eq!(corps["reason"], "albums_concurrents", "{corps}");
    let ids: Vec<&str> = corps["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .filter_map(|c| c["release_id"].as_str())
        .collect();
    assert_eq!(ids, ["rel-xxiv", MBID_OCEAN], "{corps}");
    assert_eq!(
        colonne(&state, release, 2),
        None,
        "un album ambigu ne reçoit rien"
    );

    // 2. L'édition choisie est posée, sans nouvelle recherche.
    let recherches = nb_prefixe(&compteur, "recherche:");
    let (status, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/library/albums/2/reidentify?release_id={MBID_OCEAN}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["verdict"], "reidentified", "{corps}");
    assert_eq!(corps["source"], "choix_utilisateur", "{corps}");
    assert_eq!(corps["tracks_matched"], 2, "{corps}");
    assert_eq!(colonne(&state, release, 2).as_deref(), Some(MBID_OCEAN));
    assert_eq!(nb_prefixe(&compteur, "recherche:"), recherches);

    // 3. Un release_id qui n'est pas un MBID : 400, rien ne part.
    let (status, corps) = appeler(
        &app,
        "POST",
        "/api/v1/library/albums/2/reidentify?release_id=n%27importe%20quoi",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["code"], "release_id_invalide", "{corps}");
    assert_eq!(colonne(&state, release, 2).as_deref(), Some(MBID_OCEAN));

    // 4. Le bouton part des balises : MBID posé, aucune recherche.
    let recherches = nb_prefixe(&compteur, "recherche:");
    let (status, corps) = appeler(&app, "POST", "/api/v1/library/albums/1/reidentify").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["source"], "balise_release", "{corps}");
    assert_eq!(colonne(&state, release, 1).as_deref(), Some(MBID_BALISE));
    assert_eq!(nb_prefixe(&compteur, "recherche:"), recherches);
}

/// #4767 / #4805 E — la ré-identification qui CHANGE de pressage remet
/// `albums.credits_mb_at` à NULL : la passe des crédits remplacera ceux de
/// l'ancien pressage. Retomber sur le même pressage n'y touche pas.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changer_de_pressage_remet_les_credits_a_refaire() {
    let _serie = SERIE.lock().await;
    let _compteur = doublure().await;
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    for requete in [
        // 1 — identifié comme Ocean, crédits déjà passés ; ses balises disent
        //     un autre pressage.
        "INSERT INTO albums (id, title, source, musicbrainz_release_id, credits_mb_at) \
         VALUES (1, 'Dossier 2003', 'local', '0c2a6c47-6f5e-4a54-9d3e-2d3f0b9a1c11', \
         '2026-10-01T00:00:00Z')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (10, 'Un', 1, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (11, 'Deux', 1, 'local', 2, 1)",
        // 2 — témoin : déjà sur le pressage de ses balises.
        "INSERT INTO albums (id, title, source, musicbrainz_release_id, credits_mb_at) \
         VALUES (2, 'Dossier 2004', 'local', '5b11f4ce-a62d-471e-81fc-a69a8278c7da', \
         '2026-10-01T00:00:00Z')",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (20, 'Un', 2, 'local', 1, 1)",
        "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
         VALUES (21, 'Deux', 2, 'local', 2, 1)",
    ] {
        state.backend.execute(requete, &[]).unwrap();
    }
    for piste in [10i64, 11, 20, 21] {
        state
            .backend
            .execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'mb_release_id', ?)",
                &[&piste as &dyn ToSqlValue, &MBID_BALISE as &dyn ToSqlValue],
            )
            .unwrap();
    }
    let app = tune_server::routes::router(state.clone());
    let release = "SELECT musicbrainz_release_id FROM albums WHERE id = ?";
    let credits = "SELECT credits_mb_at FROM albums WHERE id = ?";

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/albums/1/reidentify").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["verdict"], "reidentified", "{corps}");
    assert_eq!(colonne(&state, release, 1).as_deref(), Some(MBID_BALISE));
    assert_eq!(
        colonne(&state, credits, 1),
        None,
        "un autre pressage : les crédits sont à refaire"
    );

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/albums/2/reidentify").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["verdict"], "unchanged", "{corps}");
    assert_eq!(
        colonne(&state, credits, 2).as_deref(),
        Some("2026-10-01T00:00:00Z"),
        "même pressage : les crédits restent"
    );
}
