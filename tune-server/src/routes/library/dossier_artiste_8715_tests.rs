//! Routes réelles, base synthétique, aucun fichier audio ni service extérieur.
use crate::state::AppState;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::Value;
use std::collections::BTreeSet;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

fn banc() -> (axum::Router, String) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let db = &state.backend;
    db.execute(
        "INSERT INTO artists(id,name) VALUES (1,'Artiste 8715'),(2,'Autre artiste')",
        &[],
    )
    .unwrap();
    let dossier = "/Banc8715/Été_100%".replace('/', std::path::MAIN_SEPARATOR_STR);
    for id in 1i64..=12 {
        let principal = if id <= 4 { 1i64 } else { 2i64 };
        let interprete = if id <= 8 { 1i64 } else { 2i64 };
        let compilation = i64::from(id == 5 || id == 6);
        let chemin = match id {
            1 => format!("{dossier}/album/piste.flac"),
            3 => format!("{dossier}/sous-dossier/album/piste.flac"),
            4 => "/Banc8715/ÉtéX1000/voisin.flac".into(),
            n if n % 2 == 1 => format!("{dossier}/{n}/piste.flac"),
            n => format!("/Banc8715/ailleurs/{n}/piste.flac"),
        }
        .replace('/', std::path::MAIN_SEPARATOR_STR);
        let titre = format!("Album {id}");
        db.execute("INSERT INTO albums(id,title,artist_id,source,track_count,is_compilation) VALUES (?,?,?,'local',1,?)",
            &[&id as &dyn ToSqlValue, &titre, &principal, &compilation]).unwrap();
        let titre = format!("Piste {id}");
        db.execute("INSERT INTO tracks(id,title,album_id,artist_id,file_path,file_mtime,duration_ms,disc_number,track_number,format,sample_rate,bit_depth) VALUES (?,?,?,?,?,1000.0,30000,1,1,'flac',44100,16)",
            &[&id as &dyn ToSqlValue, &titre, &id, &interprete, &chemin]).unwrap();
        if id >= 9 {
            let role = if id <= 10 { "performer" } else { "composer" };
            db.execute("INSERT INTO track_credits(track_id,artist_id,artist_name,role) VALUES (?,1,'Artiste 8715',?)", &[&id as &dyn ToSqlValue, &role]).unwrap();
        }
    }
    db.execute("INSERT INTO tracks(id,title,album_id,artist_id,file_path,file_mtime,duration_ms,disc_number,track_number,format,sample_rate,bit_depth) VALUES (100,'Piste 1',1,1,'/ailleurs/meilleure.flac',1000.0,30000,1,1,'flac',192000,24)", &[]).unwrap();
    (crate::routes::router(state), dossier)
}
async fn lire(app: &axum::Router, uri: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK, "{uri}");
    serde_json::from_slice(
        &axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn ids(v: &Value) -> BTreeSet<i64> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["id"].as_i64())
        .collect()
}
#[tokio::test]
async fn les_albums_de_la_fiche_restent_dans_le_dossier_8715() {
    let (app, dossier) = banc();
    let prefixe = "/api/v1/library/artists/1/albums";
    let tous = lire(&app, prefixe).await;
    assert!(ids(&tous).contains(&2));
    assert!(ids(&tous).contains(&4));
    let uri = format!("{prefixe}?folder={}", urlencoding::encode(&dossier));
    assert_eq!(
        ids(&lire(&app, &uri).await),
        BTreeSet::from([1, 3]),
        "la fiche artiste doit limiter les albums au sous-arbre exact, sans interpréter % ou _"
    );
    assert_eq!(
        ids(&lire(&app, &format!("{prefixe}?folder=")).await),
        ids(&tous)
    );
}
#[tokio::test]
async fn les_participations_respectent_aussi_le_dossier_8715() {
    let (app, dossier) = banc();
    let uri = format!(
        "/api/v1/library/artists/1/albums?sections=1&folder={}",
        urlencoding::encode(&dossier)
    );
    let fiche = lire(&app, &uri).await;
    assert_eq!(ids(&fiche["albums"]), BTreeSet::from([1, 3]));
    assert_eq!(
        ids(&fiche["compilations"]),
        BTreeSet::from([5]),
        "les compilations hors du dossier doivent disparaître"
    );
    assert_eq!(ids(&fiche["appearances"]), BTreeSet::from([7]));
    assert_eq!(
        ids(&fiche["collaborations"][0]["albums"]),
        BTreeSet::from([9])
    );
    assert_eq!(ids(&fiche["covers"]), BTreeSet::from([11]));
    assert_eq!(ids(&fiche["writing"]), BTreeSet::from([11]));
}
#[tokio::test]
async fn les_pistes_sont_filtrees_avant_les_copies_de_meilleure_qualite_8715() {
    let (app, dossier) = banc();
    let prefixe = "/api/v1/library/artists/1/tracks";
    let toutes = lire(&app, prefixe).await;
    assert!(
        ids(&toutes).contains(&100),
        "le témoin non filtré garde la meilleure copie"
    );
    let uri = format!("{prefixe}?folder={}", urlencoding::encode(&dossier));
    assert_eq!(
        ids(&lire(&app, &uri).await),
        BTreeSet::from([1, 3, 5, 7]),
        "les gestes de lecture doivent garder la copie du dossier, avant dédoublonnage"
    );
}
