//! #8083 — ajouter un dossier de musique relançait le scan de TOUTE la
//! bibliothèque (730 000 pistes reparcourues pour 3 000 nouvelles) :
//! `add_music_dir` passait `None` à `spawn_library_scan_par`.
//!
//! L'épreuve joue la VRAIE route d'ajout sur de vrais FLAC et une base de
//! fichier : une première racine est scannée, l'un de ses fichiers disparaît
//! du disque, puis une seconde racine est ajoutée. Le scan qui part ne doit
//! parcourir que la racine ajoutée, et ne rien élaguer ailleurs — la piste
//! disparue de la première racine reste en base, comme ses voisines.
use super::*;
use crate::auth::RequireAdmin;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tune_core::db::track_repo::TrackRepo;

fn gabarit() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac")
}

/// `n` vrais FLAC, chacun avec ses balises : des copies octet pour octet du
/// même gabarit deviendraient des EXEMPLAIRES d'une seule piste (#4907), sans
/// ligne `tracks` propre.
fn pistes(dossier: &Path, n: usize) -> Vec<PathBuf> {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    std::fs::create_dir_all(dossier).unwrap();
    let album = dossier.file_name().unwrap().to_string_lossy().into_owned();
    (0..n)
        .map(|i| {
            let p = dossier.join(format!("{i:02} - piste.flac"));
            std::fs::copy(gabarit(), &p).unwrap();
            let mut f = std::fs::File::open(&p).unwrap();
            let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            drop(f);
            let mut vc = VorbisComments::default();
            let titre = format!("{album} {i}");
            let numero = (i + 1).to_string();
            for (k, v) in [
                ("TITLE", titre.as_str()),
                ("ARTIST", "Essai 8083"),
                ("ALBUMARTIST", "Essai 8083"),
                ("ALBUM", album.as_str()),
                ("TRACKNUMBER", numero.as_str()),
            ] {
                vc.insert(k.to_string(), v.to_string());
            }
            flac.set_vorbis_comments(vc);
            flac.save_to_path(&p, WriteOptions::default()).unwrap();
            p
        })
        .collect()
}

/// Attend le `library.scan.completed` et le rend.
async fn fin_du_scan(
    rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
) -> serde_json::Value {
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(180), rx.recv())
            .await
            .expect("le scan n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => return ev.data,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus fermé : {e}"),
        }
    }
}

fn chemin(p: &Path) -> String {
    tune_core::scanner::walker::normalize_path(&p.to_string_lossy())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// Le verrou des scans de test est tenu pendant toute l'épreuve, à dessein :
// le droit de scanner est global au processus (voir `serialiser_les_scans_de_test`).
#[allow(clippy::await_holding_lock)]
async fn ajouter_un_dossier_ne_scanne_que_ce_dossier_8083() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    let base = tune_core::test_scratch::scratch_dir("ajout-dossier-8083-base");
    let r = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "ajout-dossier-8083-musique",
    );
    let premiere = r.path().join("Premiere");
    let ajoutee = r.path().join("Ajoutee");
    let anciennes = pistes(&premiere, 6);
    let nouvelles = pistes(&ajoutee, 2);

    let etat = AppState::new(
        &base.path().join("tune-8083.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[chemin(&premiere)]).unwrap(),
        )
        .unwrap();

    // 1. La première racine, scannée en entier.
    let mut rx = etat.event_bus.subscribe();
    assert!(crate::routes::system::scan::spawn_library_scan(etat.clone(), false, None).await);
    let complet = fin_du_scan(&mut rx).await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    let repo = TrackRepo::with_backend(etat.backend.clone());
    for p in &anciennes {
        assert!(
            repo.get_by_path(&chemin(p)).unwrap().is_some(),
            "le premier scan doit avoir rangé {p:?} : {complet}"
        );
    }

    // 2. Un fichier de la première racine disparaît du disque. Un scan de
    //    TOUTE la bibliothèque l'élaguerait ; l'ajout d'un autre dossier n'a
    //    rien à en dire.
    std::fs::remove_file(&anciennes[0]).unwrap();

    // 3. L'ajout, par la vraie route.
    let mut rx = etat.event_bus.subscribe();
    // `AppError` n'implémente pas `Debug` : pas d'`expect`, un `match`.
    let reponse = match add_music_dir(
        RequireAdmin,
        State(etat.clone()),
        Json(AddMusicDir {
            path: ajoutee.to_string_lossy().into(),
        }),
    )
    .await
    {
        Ok(r) => r.into_response(),
        Err(e) => panic!("l'ajout doit réussir : {}", e.into_response().status()),
    };
    assert_eq!(reponse.status(), StatusCode::OK);
    let rapport = fin_du_scan(&mut rx).await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    assert_eq!(
        rapport["scope"], "folder",
        "#8083 : ajouter un dossier a relancé un scan de toute la bibliothèque : {rapport}"
    );
    assert_eq!(
        rapport["total_files"],
        serde_json::json!(nouvelles.len()),
        "#8083 : le scan de l'ajout doit parcourir les seuls fichiers du dossier ajouté : {rapport}"
    );
    for p in &nouvelles {
        assert!(
            repo.get_by_path(&chemin(p)).unwrap().is_some(),
            "#8083 : la piste ajoutée {p:?} doit être rangée : {rapport}"
        );
    }
    for p in &anciennes {
        assert!(
            repo.get_by_path(&chemin(p)).unwrap().is_some(),
            "#8083 : l'ajout d'un dossier a élagué une piste d'une AUTRE racine ({p:?}) : {rapport}"
        );
    }
}
