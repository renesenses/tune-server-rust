//! `POST /library/albums/{id}/edition/write-tags` — par la fonction de route,
//! sur de VRAIS fichiers copiés des fixtures de `tune-core`, dans un dossier
//! déclaré comme racine de la bibliothèque.
use super::*;
use serde_json::Value;
use tune_core::db::backend::DbBackend;
use tune_core::db::edition_album::Modification;
use tune_core::metadata::empreinte_audio::empreinte_audio;

fn fixture(nom: &str) -> PathBuf {
    FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures")
        .join(nom)
}

async fn appeler(state: &AppState, id: i64, corps: &str) -> (StatusCode, Value) {
    let r = ecrire_balises(
        State(state.clone()),
        Path(id),
        Bytes::from(corps.to_string()),
    )
    .await;
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

struct Banc {
    state: AppState,
    _dir: tempfile::TempDir,
    album: PathBuf,
    ailleurs: PathBuf,
}

/// Un album « Köln » de huit pistes, toutes les raisons de sauter réunies :
///
/// | id | fichier                 | attendu               |
/// |----|-------------------------|-----------------------|
/// | 11 | Koln/01.flac            | écrit                 |
/// | 12 | Koln/02.mp3             | écrit                 |
/// | 13 | Koln/03.m4a             | écrit                 |
/// | 14 | ailleurs/04.flac        | `hors_racines`        |
/// | 15 | Koln/05.flac (0444)     | `lecture_seule`       |
/// | 16 | Koln/06.dsf             | `format_non_gere`     |
/// | 17 | Koln/07.flac            | `en_lecture` (zone)   |
/// | 18 | image.flac (feuille CUE)| `piste_cue`           |
/// | 19 | (Qobuz)                 | `piste_de_service`    |
fn banc() -> Banc {
    let dir = tempfile::tempdir().unwrap();
    let racine = dir.path().join("musique");
    let album = racine.join("Koln");
    let ailleurs = dir.path().join("ailleurs");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::create_dir_all(&ailleurs).unwrap();
    for (src, dst) in [
        ("test.flac", album.join("01.flac")),
        ("test.mp3", album.join("02.mp3")),
        ("test.m4a", album.join("03.m4a")),
        ("test.flac", ailleurs.join("04.flac")),
        ("test.flac", album.join("05.flac")),
        ("dsd/ref_dsd64_stereo.dsf", album.join("06.dsf")),
        ("test.flac", album.join("07.flac")),
    ] {
        std::fs::copy(fixture(src), &dst).unwrap();
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            album.join("05.flac"),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let b = &state.backend;
    tune_core::db::settings_repo::SettingsRepo::with_backend(b.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&vec![racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    b.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Keith Jarrett'), (2, 'Gary Peacock')",
        &[],
    )
    .unwrap();
    b.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Köln', 1)",
        &[],
    )
    .unwrap();
    let chemin = |p: PathBuf| p.to_string_lossy().into_owned();
    let pistes: [(i64, &str, i64, Option<String>); 7] = [
        (11, "Part I", 1, Some(chemin(album.join("01.flac")))),
        (12, "Part IIa", 2, Some(chemin(album.join("02.mp3")))),
        (13, "Part IIb", 1, Some(chemin(album.join("03.m4a")))),
        (14, "Part IIc", 1, Some(chemin(ailleurs.join("04.flac")))),
        (15, "Coda", 1, Some(chemin(album.join("05.flac")))),
        (16, "DSD", 1, Some(chemin(album.join("06.dsf")))),
        (17, "Bis", 1, Some(chemin(album.join("07.flac")))),
    ];
    for (n, (id, titre, artiste, fichier)) in pistes.iter().enumerate() {
        b.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             duration_ms, file_path, source) VALUES (?, ?, 1, ?, 1, ?, 1000, ?, 'local')",
            &[
                id as &dyn tune_core::db::backend::ToSqlValue,
                &titre.to_string(),
                artiste,
                &((n + 1) as i64),
                fichier,
            ],
        )
        .unwrap();
    }
    b.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
         duration_ms, file_path, source, cue_media_path, cue_start_ms) \
         VALUES (18, 'Cue', 1, 1, 1, 8, 1000, NULL, 'local', ?, 0)",
        &[&chemin(album.join("image.flac")) as &dyn tune_core::db::backend::ToSqlValue],
    )
    .unwrap();
    b.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
         duration_ms, file_path, source, source_id) \
         VALUES (19, 'Service', 1, 1, 1, 9, 1000, NULL, 'qobuz', 'q-19')",
        &[],
    )
    .unwrap();
    b.execute(
        "INSERT INTO zones (id, name, last_play_state, last_track_id) \
         VALUES (1, 'Salon', 'playing', 17)",
        &[],
    )
    .unwrap();

    // Le mode « Modifier » : titre, compilation forcée, un disque nommé,
    // l'ordre des pistes changé (13 d'abord).
    let m: Modification = serde_json::from_value(json!({
        "title": "Köln 1975",
        "compilation_mode": "oui",
        "discs": [ { "number": 1, "title": "Concert",
                     "track_ids": [13, 11, 12, 14, 15, 16, 17, 18, 19] } ]
    }))
    .unwrap();
    edition_album::appliquer(&state.backend, 1, &m).unwrap();
    Banc {
        state,
        _dir: dir,
        album,
        ailleurs,
    }
}

fn raisons(v: &Value) -> Vec<(i64, String)> {
    let mut r: Vec<(i64, String)> = v["ignores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["track_id"].as_i64().unwrap(),
                i["raison"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    r.sort();
    r
}

fn octets_de(dir: &FsPath) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

fn root() -> bool {
    // SAFETY: `geteuid` ne lit que l'identité du processus.
    unsafe { libc::geteuid() == 0 }
}

#[tokio::test]
async fn dry_run_rend_le_plan_et_n_ecrit_rien() {
    let banc = banc();
    let avant = octets_de(&banc.album);
    let (s, v) = appeler(&banc.state, 1, r#"{ "dry_run": true }"#).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["ecrits"], 0);
    assert_eq!(v["a_ecrire"], 3, "{v}");
    assert_eq!(octets_de(&banc.album), avant, "le dry_run a écrit");

    let mut attendu = vec![
        (14, "hors_racines".to_string()),
        (16, "format_non_gere".to_string()),
        (17, "en_lecture".to_string()),
        (18, "piste_cue".to_string()),
        (19, "piste_de_service".to_string()),
    ];
    if !root() {
        attendu.push((15, "lecture_seule".to_string()));
    }
    attendu.sort();
    let mut vues = raisons(&v);
    if root() {
        vues.retain(|(id, _)| *id != 15);
    }
    assert_eq!(vues, attendu, "{v}");

    // Le plan de 13 : il devient la piste 1 du disque « Concert ».
    let plan13 = v["plan"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["track_id"] == 13)
        .expect("plan de 13");
    let champ = |nom: &str| {
        plan13["changements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["champ"] == nom)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(champ("ALBUM")["apres"], "Köln 1975");
    assert_eq!(champ("TRACKNUMBER")["apres"], "1");
    assert_eq!(champ("DISCSUBTITLE")["apres"], "Concert");
    assert_eq!(champ("COMPILATION")["apres"], "1");
}

#[tokio::test]
async fn ecrire_puis_relire_base_et_fichiers_concordent() {
    let banc = banc();
    let empreintes: Vec<_> = ["01.flac", "02.mp3", "03.m4a"]
        .iter()
        .map(|n| empreinte_audio(&banc.album.join(n)).unwrap().unwrap())
        .collect();
    let intouches = [
        (
            banc.ailleurs.join("04.flac"),
            std::fs::read(banc.ailleurs.join("04.flac")).unwrap(),
        ),
        (
            banc.album.join("05.flac"),
            std::fs::read(banc.album.join("05.flac")).unwrap(),
        ),
        (
            banc.album.join("07.flac"),
            std::fs::read(banc.album.join("07.flac")).unwrap(),
        ),
    ];

    let (s, v) = appeler(&banc.state, 1, "").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["ecrits"], 3, "{v}");
    assert_eq!(v["erreurs"], json!([]), "{v}");

    // Les balises, relues par le lecteur du scan.
    for (nom, numero, titre) in [
        ("03.m4a", 1, "Part IIb"),
        ("01.flac", 2, "Part I"),
        ("02.mp3", 3, "Part IIa"),
    ] {
        let m = tune_core::metadata::read_metadata(&banc.album.join(nom)).unwrap();
        assert_eq!(m.album.as_deref(), Some("Köln 1975"), "{nom}");
        assert_eq!(m.album_artist.as_deref(), Some("Keith Jarrett"), "{nom}");
        assert_eq!(m.title.as_deref(), Some(titre), "{nom}");
        assert_eq!(m.track_number, Some(numero), "{nom}");
        assert_eq!(m.total_tracks, Some(9), "{nom}");
        assert_eq!(m.disc_number, Some(1), "{nom}");
        assert_eq!(m.total_discs, Some(1), "{nom}");
        assert_eq!(m.disc_subtitle.as_deref(), Some("Concert"), "{nom}");
        assert_eq!(m.compilation, Some(true), "{nom}");
    }
    // 12 est de Gary Peacock : l'artiste de piste suit la base.
    let m = tune_core::metadata::read_metadata(&banc.album.join("02.mp3")).unwrap();
    assert_eq!(m.artist.as_deref(), Some("Gary Peacock"));

    // L'audio : empreinte identique.
    for (n, nom) in ["01.flac", "02.mp3", "03.m4a"].iter().enumerate() {
        assert_eq!(
            empreinte_audio(&banc.album.join(nom)).unwrap().unwrap(),
            empreintes[n],
            "{nom} : audio modifié"
        );
    }
    // Les fichiers sautés n'ont pas bougé d'un octet.
    for (p, o) in &intouches {
        assert_eq!(&std::fs::read(p).unwrap(), o, "{}", p.display());
    }
    // Aucune copie de travail ne traîne.
    assert!(
        octets_de(&banc.album)
            .iter()
            .all(|(n, _)| !n.starts_with(tag_writer::PREFIXE_COPIE_DE_TRAVAIL))
    );

    // La base, relue : la disposition tenue reste, la taille suit le fichier.
    let vue = edition_album::lire_vue(&banc.state.backend, 1)
        .unwrap()
        .unwrap();
    assert_eq!(vue.album.title, "Köln 1975");
    assert!(vue.ecriture_balises, "la sonde du bouton web");
    assert!(vue.album.champs_edites.contains(&"discs".to_string()));
    assert!(
        vue.album
            .champs_edites
            .contains(&"compilation_mode".to_string())
    );
    assert_eq!(vue.tracks[0].id, 13);
    assert_eq!(vue.discs[0].title.as_deref(), Some("Concert"));
    let t = tune_core::db::track_repo::TrackRepo::with_backend(banc.state.backend.clone())
        .get(11)
        .unwrap()
        .unwrap();
    assert_eq!(t.track_number, 2);
    assert_eq!(t.disc_subtitle.as_deref(), Some("Concert"));
    assert_eq!(
        t.file_size,
        Some(std::fs::metadata(banc.album.join("01.flac")).unwrap().len() as i64)
    );

    // Une seconde écriture n'a plus rien à faire.
    let (s, v) = appeler(&banc.state, 1, "{}").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ecrits"], 0, "{v}");
    assert_eq!(v["inchanges"], 3, "{v}");
}

/// Contre-épreuve de la garde de racine : sans dossier de musique déclaré,
/// RIEN n'est écrivable — même un fichier bel et bien présent.
#[tokio::test]
async fn sans_racine_declaree_tout_est_hors_racines() {
    let banc = banc();
    tune_core::db::settings_repo::SettingsRepo::with_backend(banc.state.backend.clone())
        .set("music_dirs", "[]")
        .unwrap();
    let avant = octets_de(&banc.album);
    let (s, v) = appeler(&banc.state, 1, "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ecrits"], 0);
    let hors: Vec<i64> = raisons(&v)
        .into_iter()
        .filter(|(_, r)| r == "hors_racines")
        .map(|(id, _)| id)
        .collect();
    for id in [11, 12, 13] {
        assert!(hors.contains(&id), "{id} : {v}");
    }
    assert_eq!(octets_de(&banc.album), avant);
}

/// Contre-épreuve de la garde de lecture : la zone arrêtée, 17 s'écrit.
#[tokio::test]
async fn zone_arretee_la_piste_redevient_ecrivable() {
    let banc = banc();
    banc.state
        .backend
        .execute(
            "UPDATE zones SET last_play_state = 'stopped' WHERE id = 1",
            &[],
        )
        .unwrap();
    let (_, v) = appeler(&banc.state, 1, r#"{"dry_run":true}"#).await;
    assert!(!raisons(&v).iter().any(|(id, _)| *id == 17), "{v}");
    assert!(
        v["plan"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["track_id"] == 17),
        "{v}"
    );
}

#[tokio::test]
async fn album_inconnu_404_et_corps_invalide_422() {
    let banc = banc();
    let (s, v) = appeler(&banc.state, 999, "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "album_inconnu");
    let (s, v) = appeler(&banc.state, 1, "pas du json").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"], "corps_invalide");
}
