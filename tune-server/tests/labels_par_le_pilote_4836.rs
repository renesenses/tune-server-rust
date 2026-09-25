//! #4836 — le pilote d'identification remplit le LABEL des albums.
//!
//! **Hermétique : aucun appel réel à MusicBrainz.** Une doublure locale
//! (127.0.0.1, port éphémère) sert des réponses JSON enregistrées, compte
//! chaque requête reçue, et remplace la base MusicBrainz par
//! `remplacer_la_base_musicbrainz`. Le limiteur partagé reste en place : les
//! passes tournent à leur vrai débit (une requête par seconde).
//!
//! Les quatre témoins de l'issue :
//! 1. un album identifié reçoit le label de SA release — pas celui de la
//!    première release de la recherche, pas le pseudo-label « [no label] » ;
//! 2. un label déjà posé n'est jamais écrasé ;
//! 3. la passe « labels seulement » comble un album déjà identifié sans
//!    refaire l'identification (zéro recherche, une lecture `inc=labels`) ;
//! 4. une release sans label laisse l'album vide, sans erreur.

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

/// Les requêtes reçues par la doublure, par clé : `recherche:<titre>`,
/// `release/<id>?inc=<inc>`, `recording`.
type Compteur = Arc<Mutex<HashMap<String, usize>>>;

/// Les deux témoins partagent la base MusicBrainz remplacée (globale) : ils
/// passent l'un après l'autre.
static SERIE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn compter(c: &Compteur, cle: String) {
    *c.lock().unwrap().entry(cle).or_insert(0) += 1;
}

fn nb(c: &Compteur, cle: &str) -> usize {
    c.lock().unwrap().get(cle).copied().unwrap_or(0)
}

fn nb_prefixe(c: &Compteur, prefixe: &str) -> usize {
    c.lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with(prefixe))
        .map(|(_, n)| n)
        .sum()
}

fn release_de_recherche(id: &str, titre: &str, score: i64, pistes: u64, label: &str) -> Value {
    json!({
        "id": id,
        "score": score,
        "title": titre,
        "status": "Official",
        "track-count": pistes,
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }],
        "release-group": { "id": format!("rg-{id}") },
        "label-info": [{ "catalog-number": "X-1", "label": { "name": label } }],
    })
}

/// Les réponses enregistrées. La forme est celle de `/ws/2`, réduite aux
/// champs lus.
fn reponse_recherche(requete: &str) -> Value {
    if requete.contains("Kind of Blue") {
        // `releases[0]` est une édition QUELCONQUE, avec son propre label.
        // L'album a une piste : le classement retient `rel-sa` (meilleur
        // score). C'est SON label qui doit être écrit.
        json!({ "releases": [
            release_de_recherche("rel-premiere", "Kind of Blue", 90, 12, "Label de la premiere"),
            release_de_recherche("rel-sa", "Kind of Blue", 100, 1, "Columbia (index de recherche)"),
        ]})
    } else if requete.contains("Blue Train") {
        json!({ "releases": [release_de_recherche("rel-bt", "Blue Train", 100, 1, "Blue Note")] })
    } else if requete.contains("Sans Label") {
        json!({ "releases": [release_de_recherche("rel-vide", "Sans Label", 100, 1, "Inutile")] })
    } else {
        json!({ "releases": [] })
    }
}

fn reponse_release(id: &str) -> Option<Value> {
    let label_info = match id {
        // La release identifiée déclare d'abord « [no label] », puis son vrai
        // label : la règle prend le premier VRAI label, avec son catalogue.
        "rel-sa" => json!([
            { "catalog-number": "[none]", "label": {
                "id": "157afde4-4bf5-4039-8ad2-5a15acc85176", "name": "[no label]" } },
            { "catalog-number": "CL 1355", "label": { "id": "l-col", "name": "Columbia" } },
        ]),
        "rel-bt" => json!([{ "catalog-number": "BLP 1577", "label": { "name": "Blue Note" } }]),
        "rel-vide" => json!([]),
        "rel-lab-1" => json!([{ "catalog-number": "ECM 1064", "label": { "name": "ECM" } }]),
        "rel-lab-2" => json!([{ "label": { "name": "Ne doit pas être lu" } }]),
        "rel-lab-3" => json!([{ "catalog-number": "IMP 1", "label": { "name": "Impulse!" } }]),
        _ => return None,
    };
    Some(json!({
        "id": id,
        "title": "Titre",
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }],
        "label-info": label_info,
        "media": [{ "position": 1, "tracks": [
            { "position": 1, "title": "Piste", "recording": { "id": format!("rec-{id}") } }
        ]}],
    }))
}

/// Démarre la doublure et y fait pointer MusicBrainz.
async fn doublure() -> Compteur {
    let compteur: Compteur = Arc::default();
    let c1 = compteur.clone();
    let c2 = compteur.clone();
    let c3 = compteur.clone();
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
            axum::routing::get(
                move |Path(id): Path<String>, Query(q): Query<HashMap<String, String>>| {
                    let c = c2.clone();
                    async move {
                        let inc = q.get("inc").cloned().unwrap_or_default();
                        compter(&c, format!("release/{id}?inc={inc}"));
                        match reponse_release(&id) {
                            Some(v) => axum::Json(v).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                },
            ),
        )
        .route(
            "/recording",
            axum::routing::get(move || {
                let c = c3.clone();
                async move {
                    compter(&c, "recording".to_string());
                    axum::Json(json!({ "recordings": [] }))
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

async fn etat_premium() -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    state
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

/// Attend la fin de la passe (au plus 60 s) et rend son dernier état.
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

fn executer(state: &tune_server::state::AppState, sql: &[&str]) {
    for requete in sql {
        state.backend.execute(requete, &[]).unwrap();
    }
}

fn album(
    state: &tune_server::state::AppState,
    id: i64,
) -> (Option<String>, Option<String>, Option<String>) {
    let row = state
        .backend
        .query_one(
            "SELECT label, catalog_number, musicbrainz_release_id FROM albums WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    (row[0].as_string(), row[1].as_string(), row[2].as_string())
}

/// Témoins 1, 2 et 4, par le pilote d'identification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_pilote_pose_le_label_de_sa_release_sans_rien_ecraser() {
    let _serie = SERIE.lock().await;
    let compteur = doublure().await;
    let state = etat_premium().await;
    executer(
        &state,
        &[
            // 1 — label VIDE (ce qu'écrit un scan dont la balise est vide) :
            //     un trou, à combler par le label de rel-sa.
            "INSERT INTO albums (id, title, source, label) VALUES (1, 'Kind of Blue', 'local', '')",
            "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
             VALUES (10, 'Piste', 1, 'local', 1, 1)",
            // 2 — label posé par l'utilisateur : jamais écrasé.
            "INSERT INTO albums (id, title, source, label) \
             VALUES (2, 'Blue Train', 'local', 'Label de l''utilisateur')",
            "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
             VALUES (20, 'Piste', 2, 'local', 1, 1)",
            // 3 — la release identifiée n'a aucun label.
            "INSERT INTO albums (id, title, source) VALUES (3, 'Sans Label', 'local')",
            "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
             VALUES (30, 'Piste', 3, 'local', 1, 1)",
        ],
    );
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    let etat = attendre_la_fin(&app).await;
    assert_eq!(etat["status"], "done", "{etat}");
    assert_eq!(etat["identifies"], 3, "{etat}");

    // Témoin 1 : le label de SA release (rel-sa), premier vrai label, avec
    // son catalogue — ni « Label de la premiere » (releases[0]), ni l'index
    // de recherche, ni « [no label] ».
    assert_eq!(
        album(&state, 1),
        (
            Some("Columbia".to_string()),
            Some("CL 1355".to_string()),
            Some("rel-sa".to_string())
        ),
        "témoin 1"
    );
    // Témoin 2 : identifié, mais le label de l'utilisateur est intact.
    let (label2, _, rel2) = album(&state, 2);
    assert_eq!(rel2.as_deref(), Some("rel-bt"));
    assert_eq!(
        label2.as_deref(),
        Some("Label de l'utilisateur"),
        "témoin 2"
    );
    // Témoin 4 : identifié, sans label, sans erreur.
    let (label3, _, rel3) = album(&state, 3);
    assert_eq!(rel3.as_deref(), Some("rel-vide"));
    assert!(
        label3.as_deref().unwrap_or("").is_empty(),
        "témoin 4 : {label3:?}"
    );

    // Coût : le label n'ajoute AUCUNE requête — une recherche et un détail
    // par album, et jamais la recherche d'enregistrement de l'enrichissement.
    assert_eq!(nb_prefixe(&compteur, "recherche:"), 3);
    assert_eq!(nb_prefixe(&compteur, "release/"), 3);
    assert_eq!(nb(&compteur, "recording"), 0);
}

/// Témoin 3 (et 2, 4 dans ce mode) : la passe « labels seulement ».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_passe_labels_comble_l_identifie_sans_le_reidentifier() {
    let _serie = SERIE.lock().await;
    let compteur = doublure().await;
    let state = etat_premium().await;
    executer(
        &state,
        &[
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (10, 'Déjà identifié', 'local', 'rel-lab-1')",
            "INSERT INTO albums (id, title, source, musicbrainz_release_id, label) \
             VALUES (11, 'Déjà labellisé', 'local', 'rel-lab-2', 'Label posé')",
            "INSERT INTO albums (id, title, source, musicbrainz_release_id, label) \
             VALUES (12, 'Label vide', 'local', 'rel-lab-3', '')",
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (13, 'Release sans label', 'local', 'rel-vide')",
            // Non identifié : hors de cette passe.
            "INSERT INTO albums (id, title, source) VALUES (14, 'Kind of Blue', 'local')",
            "INSERT INTO tracks (id, title, album_id, source) VALUES (140, 'P', 14, 'local')",
        ],
    );
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all?mode=labels").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    assert_eq!(corps["total"], 3, "{corps}");
    let etat = attendre_la_fin(&app).await;
    assert_eq!(etat["status"], "done", "{etat}");
    assert_eq!(etat["mode"], "labels", "{etat}");
    assert_eq!(etat["labels_poses"], 2, "{etat}");
    assert_eq!(etat["sans_label"], 1, "{etat}");

    assert_eq!(
        album(&state, 10),
        (
            Some("ECM".to_string()),
            Some("ECM 1064".to_string()),
            Some("rel-lab-1".to_string())
        )
    );
    assert_eq!(album(&state, 11).0.as_deref(), Some("Label posé"));
    assert_eq!(album(&state, 12).0.as_deref(), Some("Impulse!"));
    assert_eq!(album(&state, 13).0, None, "release sans label : album vide");
    assert_eq!(
        album(&state, 14).2,
        None,
        "le non-identifié n'est pas touché"
    );

    // Pas de ré-identification : zéro recherche, UNE lecture `inc=labels` par
    // album candidat, aucune lecture de l'album déjà labellisé.
    assert_eq!(nb_prefixe(&compteur, "recherche:"), 0, "{:?}", compteur);
    assert_eq!(nb(&compteur, "release/rel-lab-1?inc=labels"), 1);
    assert_eq!(nb(&compteur, "release/rel-lab-3?inc=labels"), 1);
    assert_eq!(nb(&compteur, "release/rel-vide?inc=labels"), 1);
    assert_eq!(nb_prefixe(&compteur, "release/rel-lab-2"), 0);
    assert_eq!(nb_prefixe(&compteur, "release/"), 3);
}

/// Un mode inconnu est refusé, il ne retombe pas sur trois heures
/// d'identification.
#[tokio::test]
async fn un_mode_inconnu_est_refuse() {
    let state = etat_premium().await;
    let app = tune_server::routes::router(state);
    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all?mode=tout").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["code"], "mode_inconnu");
}
