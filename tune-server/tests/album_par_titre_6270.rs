//! #6270 — l'album d'une piste modifié par son TITRE était perdu, comme
//! l'artiste avant #6245.
//!
//! `edit_track` rangeait le titre dans `track.album_title`, que `tracks` n'a
//! pas, et tenait l'ANCIEN `album_id` ; le tagger faisait `UPDATE tracks SET
//! album_title …`, erreur avalée. Le titre passe désormais par
//! `metadata::album_par_titre` : l'album de ce titre chez l'artiste de
//! l'album actuel (à défaut celui de la piste), sinon un nouveau.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lofty::tag::{Accessor, TagExt};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::champs_tenus;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn artiste(state: &AppState, nom: &str) -> i64 {
    ArtistRepo::with_backend(state.backend.clone())
        .create(&Artist::new(nom.into()))
        .expect("insert artist")
}

/// Une piste de l'artiste `artist_id`, au chemin `chemin` (aucun fichier
/// n'est ouvert tant que l'écriture dans les fichiers est désactivée, ce qui
/// est le réglage par défaut).
fn piste(state: &AppState, titre: &str, artist_id: i64, chemin: &str) -> i64 {
    let mut t = Track::new(titre.into());
    t.artist_id = Some(artist_id);
    t.file_path = Some(chemin.into());
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insert track")
}

fn relis(state: &AppState, id: i64) -> Track {
    TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .expect("lecture piste")
        .expect("piste presente")
}

async fn appel(state: &AppState, methode: &str, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let app = tune_server::routes::router(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&octets).to_string()));
    (statut, json)
}

/// L'`album_id` que la tenue de la piste fige.
fn album_tenu(state: &AppState, id: i64) -> Option<i64> {
    champs_tenus::de_la_piste(&state.backend, id).and_then(|t| t.album_id)
}

fn album(state: &AppState, titre: &str, artist_id: i64) -> i64 {
    let mut a = Album::new(titre.into());
    a.artist_id = Some(artist_id);
    AlbumRepo::with_backend(state.backend.clone())
        .create(&a)
        .expect("insert album")
}

/// L'id de l'album de ce titre chez cet artiste.
fn album_de(state: &AppState, titre: &str, artist_id: i64) -> Option<i64> {
    AlbumRepo::with_backend(state.backend.clone())
        .get_by_title_and_artist(titre, artist_id, None)
        .unwrap()
        .and_then(|a| a.id)
}

fn nombre_d_albums(state: &AppState, titre: &str) -> i64 {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM albums WHERE LOWER(title) = LOWER(?)",
            &[&titre as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap()
}

/// Une piste de `artist_id`, dans l'album `album_id`.
fn piste_d_album(
    state: &AppState,
    titre: &str,
    artist_id: i64,
    album_id: i64,
    chemin: &str,
) -> i64 {
    let id = piste(state, titre, artist_id, chemin);
    state
        .backend
        .execute(
            "UPDATE tracks SET album_id = ? WHERE id = ?",
            &[&album_id as &dyn ToSqlValue, &id],
        )
        .unwrap();
    id
}

/// Les titres qu'une règle de playlist « <champ> contient » retient.
async fn regle_contient(state: &AppState, champ: &str, motif: &str) -> Vec<String> {
    let (st, r) = appel(
        state,
        "POST",
        "/api/v1/library/smart-playlists/preview",
        json!({"rules": [{"field": champ, "op": "contains", "value": motif}], "match_mode": "all"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    r["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Un FLAC dont la balise ALBUM dit `album`.
fn flac(chemin: &std::path::Path, album: &str) {
    let mut enc = tune_core::audio::encoder::AudioEncoder::new("flac", 44100, 16, 2);
    enc.start_sync().unwrap();
    enc.write_sync(&vec![0; 4096]).unwrap();
    std::fs::write(chemin, enc.finish_sync().unwrap()).unwrap();
    let mut tag = lofty::tag::Tag::new(lofty::tag::TagType::VorbisComments);
    tag.set_title("So What".into());
    tag.set_artist("Miles Davis".into());
    tag.set_album(album.into());
    tag.save_to_path(chemin, lofty::config::WriteOptions::default())
        .unwrap();
}

#[tokio::test]
async fn l_album_edite_par_son_titre_est_enregistre_et_tenu() {
    let state = etat();
    let miles = artiste(&state, "Miles Davis");
    let ancien = album(&state, "Ancien Album", miles);
    let tid = piste_d_album(&state, "So What", miles, ancien, "/m/6270/a.flac");

    for (methode, route) in [
        ("PUT", format!("/api/v1/library/tracks/{tid}")),
        ("POST", format!("/api/v1/metadata/tracks/{tid}/edit")),
        ("PATCH", format!("/api/v1/metadata/tracks/{tid}")),
    ] {
        state
            .backend
            .execute(
                "UPDATE tracks SET album_id = ? WHERE id = ?",
                &[&ancien as &dyn ToSqlValue, &tid],
            )
            .unwrap();
        let (st, corps) = appel(&state, methode, &route, json!({"album": "Kind of Blue"})).await;
        assert_eq!(st, StatusCode::OK, "{route} : {corps}");
        let nouveau = album_de(&state, "Kind of Blue", miles)
            .expect("l'album nommé doit exister, chez l'artiste de l'ancien album");
        let relue = relis(&state, tid);
        assert_eq!(
            relue.album_id,
            Some(nouveau),
            "{route} : la route a répondu OK, la piste DOIT porter l'album nommé"
        );
        assert_eq!(
            relue.album_title.as_deref(),
            Some("Kind of Blue"),
            "{route}"
        );
        assert_eq!(
            album_tenu(&state, tid),
            Some(nouveau),
            "{route} : la tenue doit figer le NOUVEL album, pas l'ancien"
        );
    }
    assert_eq!(
        nombre_d_albums(&state, "Kind of Blue"),
        1,
        "créé une fois, réutilisé ensuite"
    );
    assert_eq!(
        regle_contient(&state, "album", "kind of").await,
        vec!["So What"]
    );
    assert!(regle_contient(&state, "album", "Ancien").await.is_empty());
}

#[tokio::test]
async fn un_titre_existant_chez_un_autre_artiste_n_est_pas_repris() {
    let state = etat();
    let miles = artiste(&state, "Miles Davis");
    let autre = artiste(&state, "Autre");
    let ancien = album(&state, "Ancien Album", miles);
    let chez_miles = album(&state, "Kind of Blue", miles);
    let chez_autre = album(&state, "Greatest Hits", autre);
    let tid = piste_d_album(&state, "So What", miles, ancien, "/m/6270/b.flac");

    let route = format!("/api/v1/library/tracks/{tid}");
    let (st, corps) = appel(&state, "PUT", &route, json!({"album": "  KIND OF BLUE "})).await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(
        relis(&state, tid).album_id,
        Some(chez_miles),
        "même titre, même artiste : repris"
    );
    assert_eq!(nombre_d_albums(&state, "Kind of Blue"), 1, "doublon créé");

    let (st, corps) = appel(&state, "PUT", &route, json!({"album": "Greatest Hits"})).await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    let relue = relis(&state, tid).album_id;
    assert_ne!(
        relue,
        Some(chez_autre),
        "l'album d'un AUTRE artiste ne doit pas être repris"
    );
    assert_eq!(relue, album_de(&state, "Greatest Hits", miles));

    // Un album_id envoyé avec le titre l'emporte.
    let (st, corps) = appel(
        &state,
        "PUT",
        &route,
        json!({"album": "Rien", "album_id": ancien}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(relis(&state, tid).album_id, Some(ancien));
    assert_eq!(nombre_d_albums(&state, "Rien"), 0, "album créé pour rien");
}

/// L'écriture dans les fichiers est désactivée (réglage par défaut) : le
/// fichier dit toujours « Ancien Album ». Le rescan ne doit pas défaire
/// l'édition.
#[tokio::test]
async fn un_rescan_garde_le_nouvel_album() {
    let state = etat();
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste.flac");
    flac(&chemin, "Ancien Album");
    let miles = artiste(&state, "Miles Davis");
    let ancien = album(&state, "Ancien Album", miles);
    let tid = piste_d_album(&state, "So What", miles, ancien, &chemin.to_string_lossy());

    let (st, corps) = appel(
        &state,
        "PUT",
        &format!("/api/v1/library/tracks/{tid}"),
        json!({"album": "Kind of Blue"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    let (st, corps) = appel(
        &state,
        "POST",
        &format!("/api/v1/library/tracks/{tid}/rescan"),
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["album"], "Ancien Album",
        "le fichier n'a pas été relu : {corps}"
    );
    assert_eq!(
        relis(&state, tid).album_id,
        album_de(&state, "Kind of Blue", miles),
        "le rescan a remis l'ancien album : la tenue ne fige pas la nouvelle valeur"
    );
    assert_eq!(
        regle_contient(&state, "album", "kind").await,
        vec!["So What"]
    );

    // `rescan_track` ne relie pas l'album de la balise ; une analyse
    // complète, si. Elle reconstruit la ligne depuis le fichier (ici l'ANCIEN
    // album) puis repose les tenues par le crochet unique des analyses
    // (`edition_album::Tenues`) : c'est le NOUVEL album qui doit revenir.
    let mut relue = relis(&state, tid);
    relue.album_id = Some(ancien);
    relue.album_title = Some("Ancien Album".into());
    assert!(tune_core::db::edition_album::Tenues::charger(&state.backend).appliquer(&mut relue));
    assert_eq!(relue.album_id, album_de(&state, "Kind of Blue", miles));
}

#[tokio::test]
async fn le_tagger_enregistre_l_album_par_son_titre() {
    let state = etat();
    let miles = artiste(&state, "Miles Davis");
    let ancien = album(&state, "Ancien Album", miles);
    let tid = piste_d_album(&state, "So What", miles, ancien, "/m/6270/d.flac");

    let (st, corps) = appel(
        &state,
        "POST",
        "/api/v1/tagger/batch",
        json!({"track_ids": [tid], "fields": {"album": "Kind of Blue"}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(corps["success"], 1, "{corps}");
    let nouveau = album_de(&state, "Kind of Blue", miles);
    assert!(nouveau.is_some(), "l'album nommé n'existe pas : {corps}");
    assert_eq!(relis(&state, tid).album_id, nouveau);
    assert_eq!(album_tenu(&state, tid), nouveau, "tenue de l'ancien album");
    assert_eq!(
        regle_contient(&state, "album", "kind").await,
        vec!["So What"]
    );
}
