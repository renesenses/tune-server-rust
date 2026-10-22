//! #6245 — l'artiste d'une piste modifié par son NOM était perdu.
//!
//! `edit_track` (`PUT /library/tracks/{id}`, `POST /metadata/tracks/{id}/edit`,
//! `PATCH /metadata/tracks/{id}`) rangeait le nom dans `track.artist_name`,
//! que `tracks` n'a pas : la route répondait `200 ok` et rien ne changeait.
//! Pire, l'artiste était déclaré « champ tenu » avec l'ANCIEN `artist_id`,
//! que chaque analyse reposait. Le tagger (`POST /tagger/batch`) faisait
//! `UPDATE tracks SET artist_name …`, erreur avalée.
//!
//! Le nom passe désormais par le chemin de `batch_set_artist` : l'artiste de
//! ce nom (sans tenir compte de la casse), sinon un nouveau, puis
//! `artist_id`. La tenue fige la NOUVELLE valeur.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lofty::tag::{Accessor, TagExt};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::champs_tenus;
use tune_core::db::models::{Artist, Track};
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

fn nombre_d_artistes(state: &AppState, nom: &str) -> i64 {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM artists WHERE LOWER(name) = LOWER(?)",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap()
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

/// L'`artist_id` que la tenue de la piste fige.
fn artiste_tenu(state: &AppState, id: i64) -> Option<i64> {
    champs_tenus::de_la_piste(&state.backend, id).and_then(|t| t.artist_id)
}

/// Les titres qu'une règle de playlist « Artiste contient » retient.
async fn artiste_contient(state: &AppState, motif: &str) -> Vec<String> {
    let (st, r) = appel(
        state,
        "POST",
        "/api/v1/library/smart-playlists/preview",
        json!({"rules": [{"field": "artist", "op": "contains", "value": motif}], "match_mode": "all"}),
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

#[tokio::test]
async fn l_artiste_edite_par_son_nom_est_enregistre_et_tenu() {
    let state = etat();
    let ancien = artiste(&state, "Ancien Artiste");
    let tid = piste(&state, "Les ballons rouges", ancien, "/m/6245/a.flac");

    for route in [
        format!("/api/v1/library/tracks/{tid}"),
        format!("/api/v1/metadata/tracks/{tid}/edit"),
    ] {
        // Chaque route repart de l'ancien artiste.
        state
            .backend
            .execute(
                "UPDATE tracks SET artist_id = ? WHERE id = ?",
                &[&ancien as &dyn ToSqlValue, &tid],
            )
            .unwrap();
        let methode = if route.ends_with("/edit") {
            "POST"
        } else {
            "PUT"
        };
        let (st, corps) = appel(&state, methode, &route, json!({"artist": "Serge Lama"})).await;
        assert_eq!(st, StatusCode::OK, "{route} : {corps}");

        let relue = relis(&state, tid);
        let nouveau = ArtistRepo::with_backend(state.backend.clone())
            .get_by_name("Serge Lama")
            .unwrap()
            .and_then(|a| a.id)
            .expect("l'artiste nommé doit exister après l'édition");
        assert_eq!(
            relue.artist_id,
            Some(nouveau),
            "{route} : la route a répondu OK, la piste DOIT porter l'artiste nommé"
        );
        assert_eq!(relue.artist_name.as_deref(), Some("Serge Lama"), "{route}");
        assert_eq!(
            artiste_tenu(&state, tid),
            Some(nouveau),
            "{route} : la tenue doit figer le NOUVEL artiste, pas l'ancien"
        );
    }
    // Créé une fois, réutilisé ensuite.
    assert_eq!(nombre_d_artistes(&state, "Serge Lama"), 1);

    // La règle « Artiste contient » suit.
    assert_eq!(
        artiste_contient(&state, "lama").await,
        vec!["Les ballons rouges"]
    );
    assert!(artiste_contient(&state, "Ancien").await.is_empty());
}

#[tokio::test]
async fn un_nom_existant_reutilise_l_artiste_sans_doublon() {
    let state = etat();
    let ancien = artiste(&state, "Ancien Artiste");
    let lama = artiste(&state, "Serge Lama");
    let tid = piste(&state, "Les ballons rouges", ancien, "/m/6245/b.flac");

    let (st, corps) = appel(
        &state,
        "PATCH",
        &format!("/api/v1/metadata/tracks/{tid}"),
        json!({"artist": "  serge LAMA "}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(relis(&state, tid).artist_id, Some(lama));
    assert_eq!(nombre_d_artistes(&state, "Serge Lama"), 1, "doublon créé");
    assert_eq!(artiste_tenu(&state, tid), Some(lama));
}

#[tokio::test]
async fn un_artist_id_envoye_avec_le_nom_l_emporte() {
    let state = etat();
    let ancien = artiste(&state, "Ancien Artiste");
    let choisi = artiste(&state, "Choisi");
    let tid = piste(&state, "Titre", ancien, "/m/6245/c.flac");
    let (st, corps) = appel(
        &state,
        "PUT",
        &format!("/api/v1/library/tracks/{tid}"),
        json!({"artist": "Autre Nom", "artist_id": choisi}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(relis(&state, tid).artist_id, Some(choisi));
    assert_eq!(
        nombre_d_artistes(&state, "Autre Nom"),
        0,
        "artiste créé pour rien"
    );
}

/// Un FLAC dont la balise ARTIST dit `artiste`.
fn flac(chemin: &std::path::Path, artiste: &str) {
    let mut enc = tune_core::audio::encoder::AudioEncoder::new("flac", 44100, 16, 2);
    enc.start_sync().unwrap();
    enc.write_sync(&vec![0; 4096]).unwrap();
    std::fs::write(chemin, enc.finish_sync().unwrap()).unwrap();
    let mut tag = lofty::tag::Tag::new(lofty::tag::TagType::VorbisComments);
    tag.set_title("Les ballons rouges".into());
    tag.set_artist(artiste.into());
    tag.save_to_path(chemin, lofty::config::WriteOptions::default())
        .unwrap();
}

/// L'écriture dans les fichiers est désactivée (réglage par défaut) : le
/// fichier dit toujours « Ancien Artiste ». Une nouvelle lecture du fichier
/// ne doit pas défaire l'édition — c'est la tenue qui la garde, et elle doit
/// tenir le NOUVEL artiste.
#[tokio::test]
async fn un_rescan_garde_le_nouvel_artiste() {
    let state = etat();
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste.flac");
    flac(&chemin, "Ancien Artiste");
    let ancien = artiste(&state, "Ancien Artiste");
    let tid = piste(
        &state,
        "Les ballons rouges",
        ancien,
        &chemin.to_string_lossy(),
    );

    let (st, corps) = appel(
        &state,
        "PUT",
        &format!("/api/v1/library/tracks/{tid}"),
        json!({"artist": "Serge Lama"}),
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
        corps["artist"], "Ancien Artiste",
        "le fichier n'a pas été relu : {corps}"
    );

    let lama = ArtistRepo::with_backend(state.backend.clone())
        .get_by_name("Serge Lama")
        .unwrap()
        .and_then(|a| a.id);
    assert!(lama.is_some());
    assert_eq!(
        relis(&state, tid).artist_id,
        lama,
        "le rescan a remis l'ancien artiste : la tenue ne fige pas la nouvelle valeur"
    );
    assert_eq!(
        artiste_contient(&state, "lama").await,
        vec!["Les ballons rouges"]
    );
}

/// Le tagger : `POST /tagger/batch` avec un artiste. La lecture de la piste
/// interrogeait des colonnes absentes (`path`, `artist_name`, `album_title`)
/// et la route tombait en 500 ; l'UPDATE de l'artiste visait `artist_name`.
#[tokio::test]
async fn le_tagger_enregistre_l_artiste_par_son_nom() {
    let state = etat();
    let ancien = artiste(&state, "Ancien Artiste");
    let tid = piste(&state, "Les ballons rouges", ancien, "/m/6245/d.flac");

    let (st, corps) = appel(
        &state,
        "POST",
        "/api/v1/tagger/batch",
        json!({"track_ids": [tid], "fields": {"artist": "Serge Lama"}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(corps["success"], 1, "{corps}");

    let lama = ArtistRepo::with_backend(state.backend.clone())
        .get_by_name("Serge Lama")
        .unwrap()
        .and_then(|a| a.id);
    assert!(lama.is_some(), "l'artiste nommé n'existe pas : {corps}");
    assert_eq!(relis(&state, tid).artist_id, lama);
    assert_eq!(artiste_tenu(&state, tid), lama, "tenue de l'ancien artiste");
    assert_eq!(
        artiste_contient(&state, "lama").await,
        vec!["Les ballons rouges"]
    );

    // L'aperçu du tagger lit l'ancien artiste par jointure.
    let (st, corps) = appel(
        &state,
        "POST",
        "/api/v1/tagger/preview",
        json!({"track_ids": [tid], "fields": {"artist": "Autre"}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert!(corps.to_string().contains("Serge Lama"), "{corps}");
}
