//! #6124 — la date de sortie originale et la version sur les surfaces de
//! pistes et sur la fiche d'album (concordance des pistes), et la liste des
//! balises réellement présentes dans le fichier (`…/file-tags`), par le VRAI
//! routeur.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::state::AppState;

/// | album | pistes | `track_metadata` |
/// |---|---|---|
/// | 10 (sans date) | 101, 102 | même `original_date` ; versions différentes |
/// | 11 (`original_date = 1970`) | 103, 104 | 103 seule porte `1971` |
/// | 12 | 105, 106 | même `version = Live` |
fn bibliotheque() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
             INSERT INTO albums (id, title, artist_id, original_date) VALUES
               (10, 'Kind of Blue', 1, NULL),
               (11, 'Bitches Brew', 1, '1970'),
               (12, 'At Fillmore', 1, NULL);
             INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, duration_ms, source) VALUES
               (101, 'So What', 10, 1, 1, '/m/kob/1.flac', 1000, 'local'),
               (102, 'Blue in Green', 10, 1, 2, '/m/kob/2.flac', 1000, 'local'),
               (103, 'Spanish Key', 11, 1, 1, '/m/bb/1.flac', 1000, 'local'),
               (104, 'Miles Runs', 11, 1, 2, '/m/bb/2.flac', 1000, 'local'),
               (105, 'Wednesday', 12, 1, 1, '/m/af/1.flac', 1000, 'local'),
               (106, 'Thursday', 12, 1, 2, '/m/af/2.flac', 1000, 'local');
             INSERT INTO track_metadata (track_id, key, value) VALUES
               (101, 'original_date', '1959-08-17'), (101, 'version', 'Remaster 2009'),
               (102, 'original_date', '1959-08-17'), (102, 'version', 'Mono'),
               (103, 'original_date', '1971'),
               (105, 'version', 'Live'), (106, 'version', 'Live');",
        )
        .expect("bibliothèque");
    (crate::routes::router(state.clone()), state)
}

async fn appel(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn get(app: &axum::Router, chemin: &str) -> Value {
    let (statut, v) = appel(app, chemin).await;
    assert_eq!(statut, StatusCode::OK, "{chemin} : {v}");
    v
}

#[tokio::test]
async fn la_piste_porte_sa_date_originale_et_sa_version_6124() {
    let (app, _state) = bibliotheque();
    let t = get(&app, "/api/v1/library/tracks/101").await;
    assert_eq!(t["original_date"], "1959-08-17", "{t}");
    assert_eq!(t["version"], "Remaster 2009", "{t}");
    // Sans balise, la clé est ABSENTE (même contrat que le DR).
    let t = get(&app, "/api/v1/library/tracks/104").await;
    assert!(t.get("original_date").is_none(), "{t}");
    assert!(t.get("version").is_none(), "{t}");

    let pistes = get(&app, "/api/v1/library/albums/10/tracks").await;
    let par_id = |id: i64| {
        pistes
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("piste {id} absente : {pistes}"))
    };
    assert_eq!(par_id(101)["version"], "Remaster 2009");
    assert_eq!(par_id(102)["version"], "Mono");
    assert_eq!(par_id(102)["original_date"], "1959-08-17");
}

#[tokio::test]
async fn l_album_prend_la_date_sur_laquelle_ses_pistes_concordent_6124() {
    let (app, _state) = bibliotheque();
    let a = get(&app, "/api/v1/library/albums/10").await;
    assert_eq!(
        a["original_date"], "1959-08-17",
        "#6124 — les deux pistes concordent : la date doit remonter à l'album. {a}"
    );
    assert_eq!(a["original_year"], 1959, "{a}");
    assert!(
        a.get("version").is_none(),
        "versions différentes : pas de version d'album. {a}"
    );

    // La date propre de l'album prime ; une piste seule ne la remplace pas.
    let a = get(&app, "/api/v1/library/albums/11").await;
    assert_eq!(a["original_date"], "1970", "{a}");

    let a = get(&app, "/api/v1/library/albums/12").await;
    assert_eq!(a["version"], "Live", "{a}");
    assert!(a["original_date"].is_null(), "{a}");
}

/// Une copie de la fixture FLAC du dépôt, balisée `VERSION` et
/// `ORIGINALRELEASEDATE` par le bloc Vorbis BRUT.
fn flac_balise() -> tune_core::test_scratch::ScratchFile {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures/test.flac");
    let copie = tune_core::test_scratch::scratch_file("file-tags-6124", ".flac");
    std::fs::copy(&source, &copie).expect("copie de la fixture");
    let mut fh = std::fs::File::open(&*copie).unwrap();
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    if flac.vorbis_comments().is_none() {
        flac.set_vorbis_comments(VorbisComments::default());
    }
    let vc = flac.vorbis_comments_mut().unwrap();
    vc.push("VERSION".into(), "Remaster 2009".into());
    vc.push("ORIGINALRELEASEDATE".into(), "1959-08-17".into());
    flac.save_to_path(&*copie, WriteOptions::default()).unwrap();
    copie
}

#[tokio::test]
async fn la_fiche_liste_les_balises_reellement_presentes_6124() {
    let (app, state) = bibliotheque();
    let fichier = flac_balise();
    let chemin = fichier.to_string_lossy().to_string();
    state
        .backend
        .execute(
            "UPDATE tracks SET file_path = ? WHERE id = 101",
            &[&chemin as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap();
    let v = get(&app, "/api/v1/library/tracks/101/file-tags").await;
    assert_eq!(v["available"], true, "{v}");
    assert_eq!(v["format"], "FLAC", "{v}");
    let balises = v["tags"].as_array().expect("tags");
    for (cle, valeur) in [
        ("VERSION", "Remaster 2009"),
        ("ORIGINALRELEASEDATE", "1959-08-17"),
    ] {
        assert!(
            balises
                .iter()
                .any(|b| b["key"] == cle && b["value"] == valeur && b["block"] == "vorbis"),
            "{cle} absente : {v}"
        );
    }
    assert_eq!(v["truncated"], false, "{v}");

    // Un fichier introuvable : `available: false`, pas une erreur.
    let v = get(&app, "/api/v1/library/tracks/102/file-tags").await;
    assert_eq!(v["available"], false, "{v}");
    assert_eq!(v["reason"], "unreadable", "{v}");

    // Une piste sans fichier local.
    state
        .backend
        .execute(
            "UPDATE tracks SET file_path = NULL, source = 'qobuz' WHERE id = 103",
            &[],
        )
        .unwrap();
    let v = get(&app, "/api/v1/library/tracks/103/file-tags").await;
    assert_eq!(v["reason"], "no_local_file", "{v}");

    let (statut, _) = appel(&app, "/api/v1/library/tracks/999/file-tags").await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}
