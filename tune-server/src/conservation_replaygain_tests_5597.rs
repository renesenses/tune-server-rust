//! #5597 (Tades, fil 2083) — « l'analyse ReplayGain redémarre de 0 ».
//!
//! La jauge, elle, repart de zéro : c'est un état de processus
//! (`replaygain::progression`), corrigé côté affichage par #5648. Reste la
//! question qui fait le P0 : des résultats DÉJÀ calculés sont-ils effacés, ou
//! recalculés, par ce qu'un utilisateur fait entre deux relevés ?
//!
//! Ces épreuves répondent sur le vrai chemin, et pas sur un fac-similé :
//!
//! - le **redémarrage** et la **mise à jour** : un nouvel `AppState` sur la
//!   même base de FICHIER, ce qui rejoue `run_migrations` (les migrations
//!   numérotées se sautent, les passes de démarrage se rejouent), puis le
//!   **scan de démarrage** (`spawn_auto_scan`) ;
//! - le **scan manuel**, simple puis complet (`spawn_library_scan`), et un
//!   fichier **retouché** que le scan relit pour de bon ;
//! - la **réanalyse** : la vraie passe `analyze_track_batch`.
//!
//! Le témoin : toutes les lignes `rg_*` et `dr_*` d'une piste mesurée, telles
//! que la passe les écrit. Elles doivent sortir de chaque étape au caractère
//! près, sur le même `tracks.id` (un nouvel identifiant, c'est une perte : les
//! lignes partent avec l'ancien par `ON DELETE CASCADE`).
use super::surveillant_retouche_tests_4896::{baliser, flac_8_canaux};
use crate::state::AppState;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;

/// Les résultats d'une piste mesurée par Tune, tels que la passe les pose
/// (`ecrire_la_mesure_de_piste`, le gain d'album, la plage dynamique).
const RESULTATS: [(&str, &str); 8] = [
    ("rg_analyzed", "1790000000"),
    ("rg_track_gain", "-6.12 dB"),
    ("rg_track_peak", "0.912345"),
    ("rg_album_gain", "-5.80 dB"),
    ("rg_album_peak", "0.987654"),
    ("dr_track", "11"),
    ("dr_album", "12"),
    ("dr_source", "analysis"),
];

struct Bibliotheque {
    _racine: tune_core::test_scratch::ScratchDir,
    base: PathBuf,
    musique: PathBuf,
    mesuree: PathBuf,
    neuve: PathBuf,
}

/// Deux pistes d'un même album, datées d'hier, SANS balise ReplayGain : tout
/// ce que la base porte sur elles vient de l'analyse de Tune. Racine sous le
/// dossier courant : `is_tune_temp_file` écarte le dossier temporaire.
fn bibliotheque(epreuve: &str) -> Bibliotheque {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("rg-5597-{epreuve}"),
    );
    let dossier = racine.path().join("Bjork").join("Homogenic");
    std::fs::create_dir_all(&dossier).unwrap();
    let hier = SystemTime::now() - Duration::from_secs(86_400);
    let mut pistes = Vec::new();
    for (n, titre) in [(1, "Hunter"), (2, "Joga")] {
        let piste = dossier.join(format!("0{n} - {titre}.flac"));
        std::fs::write(&piste, flac_8_canaux()).unwrap();
        baliser(
            &piste,
            &[
                ("TITLE", titre),
                ("ARTIST", "Bjork"),
                ("ALBUM", "Homogenic"),
                ("TRACKNUMBER", &n.to_string()),
            ],
            hier,
        );
        pistes.push(piste);
    }
    let base = racine.path().join("tune-epreuve-5597.db");
    Bibliotheque {
        mesuree: pistes[0].clone(),
        neuve: pistes[1].clone(),
        base,
        musique: racine.path().to_path_buf(),
        _racine: racine,
    }
}

/// Un démarrage du serveur : `AppState::new` ouvre la base de FICHIER et
/// rejoue `run_migrations`, exactement ce que fait une mise à jour.
fn demarrer(b: &Bibliotheque) -> AppState {
    let etat = AppState::new(&b.base.to_string_lossy(), 0, Default::default())
        .expect("AppState sur base de fichier");
    let reglages = SettingsRepo::with_backend(etat.backend.clone());
    reglages
        .set(
            "music_dirs",
            &serde_json::to_string(&[b.musique.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    // L'analyse armée, comme chez le testeur (« tags des fichiers + analyse »).
    reglages
        .set(tune_core::audio::replaygain::MODE_KEY, "album")
        .unwrap();
    etat
}

async fn scan_manuel(etat: &AppState, complet: bool) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    let mut rx = etat.event_bus.subscribe();
    let debut = Instant::now();
    while !crate::routes::system::scan::spawn_library_scan(etat.clone(), complet, None).await {
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("le scan manuel n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => {
                crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
                return;
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus d'événements fermé : {e}"),
        }
    }
}

/// Le scan de DÉMARRAGE, celui qui suit toute mise à jour. Il se retire en
/// silence quand un autre scan tient le droit : `scan_started_at` dit s'il a
/// vraiment tourné.
async fn scan_de_demarrage(db: &Arc<dyn DbBackend>) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    let reglages = SettingsRepo::with_backend(db.clone());
    let debut = Instant::now();
    loop {
        reglages.set("scan_started_at", "0").unwrap();
        let fini = crate::auto_scan::spawn_auto_scan(
            db.clone(),
            Arc::new(tune_core::event_bus::EventBus::new()),
        );
        while !fini.load(Ordering::Acquire) {
            assert!(
                debut.elapsed() < Duration::from_secs(120),
                "scan de démarrage sans fin"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if reglages.get("scan_started_at").unwrap().as_deref() != Some("0") {
            crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
            return;
        }
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn id_par_chemin(db: &Arc<dyn DbBackend>, chemin: &Path) -> i64 {
    let chemin = chemin.to_string_lossy().to_string();
    let lignes = db
        .query_many(
            "SELECT id FROM tracks WHERE file_path = ?",
            &[&chemin as &dyn tune_core::db::backend::ToSqlValue],
        )
        .expect("lecture tracks");
    assert_eq!(lignes.len(), 1, "une ligne attendue pour {chemin}");
    lignes[0][0].as_i64().expect("id")
}

fn titre(db: &Arc<dyn DbBackend>, id: i64) -> String {
    db.query_one(&format!("SELECT title FROM tracks WHERE id = {id}"), &[])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_string()))
        .unwrap_or_default()
}

/// Les lignes ReplayGain et plage dynamique d'une piste, et rien d'autre.
fn resultats(db: &Arc<dyn DbBackend>, id: i64) -> BTreeMap<String, String> {
    TrackMetadataRepo::with_backend(db.clone())
        .get_all(id)
        .expect("lecture track_metadata")
        .into_iter()
        .filter(|(k, _)| k.starts_with("rg_") || k.starts_with("dr_"))
        .collect()
}

fn attendus() -> BTreeMap<String, String> {
    RESULTATS
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn verifier(etape: &str, db: &Arc<dyn DbBackend>, b: &Bibliotheque, id_d_origine: i64) {
    let id = id_par_chemin(db, &b.mesuree);
    assert_eq!(
        id, id_d_origine,
        "{etape} : la piste mesurée a changé d'identifiant. Ses résultats ReplayGain sont \
         partis avec l'ancienne ligne (ON DELETE CASCADE) : c'est une perte réelle (#5597)"
    );
    assert_eq!(
        resultats(db, id),
        attendus(),
        "{etape} : des résultats ReplayGain / plage dynamique DÉJÀ calculés ont été effacés ou \
         réécrits (#5597)"
    );
}

/// 🔴 #5597 — le P0 : aucune perte réelle de résultats ReplayGain, ni au
/// redémarrage, ni à la mise à jour, ni au scan, ni à la réanalyse.
#[tokio::test(flavor = "multi_thread")]
async fn les_resultats_replaygain_survivent_redemarrage_scan_et_reanalyse_5597() {
    let b = bibliotheque("conservation");

    // --- Avant : la bibliothèque est scannée, une piste a été mesurée. ---
    let etat = demarrer(&b);
    scan_manuel(&etat, false).await;
    let db = etat.backend.clone();
    let id = id_par_chemin(&db, &b.mesuree);
    let champs: std::collections::HashMap<String, String> = RESULTATS
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    TrackMetadataRepo::with_backend(db.clone())
        .set_batch(id, &champs)
        .unwrap();
    assert_eq!(resultats(&db, id), attendus(), "état de départ");
    drop(db);
    drop(etat);

    // --- 1. Redémarrage = mise à jour : migrations rejouées, scan de démarrage. ---
    let etat = demarrer(&b);
    let db = etat.backend.clone();
    verifier("redémarrage (run_migrations rejoué)", &db, &b, id);
    scan_de_demarrage(&db).await;
    verifier("scan de démarrage", &db, &b, id);

    // --- 2. Scan manuel, simple puis complet. ---
    scan_manuel(&etat, false).await;
    verifier("scan manuel", &db, &b, id);
    scan_manuel(&etat, true).await;
    verifier("scan complet", &db, &b, id);

    // --- 3. Fichier retouché : le scan le RELIT (le titre change en base),
    //        sans balise ReplayGain. La mesure de Tune doit rester. ---
    baliser(
        &b.mesuree,
        &[
            ("TITLE", "Hunter (retouché)"),
            ("ARTIST", "Bjork"),
            ("ALBUM", "Homogenic"),
            ("TRACKNUMBER", "1"),
        ],
        SystemTime::now(),
    );
    scan_manuel(&etat, false).await;
    assert_eq!(
        titre(&db, id),
        "Hunter (retouché)",
        "contre-témoin : le scan doit avoir relu le fichier retouché"
    );
    verifier("scan d'un fichier retouché", &db, &b, id);

    // --- 4. Réanalyse : la vraie passe. ---
    let neuve = id_par_chemin(&db, &b.neuve);
    assert!(
        resultats(&db, neuve).is_empty(),
        "la seconde piste n'a encore rien"
    );
    tune_core::audio::replaygain::analyze_track_batch(&db).await;
    assert!(
        resultats(&db, neuve).contains_key("rg_analyzed"),
        "contre-témoin : la passe a bien tourné, elle a traité la piste qui n'avait rien. \
         Lignes : {:?}",
        resultats(&db, neuve)
    );
    verifier("réanalyse (analyze_track_batch)", &db, &b, id);
    assert_eq!(
        tune_core::audio::replaygain::compter_les_candidats_replaygain(&db),
        0,
        "plus rien à analyser : la piste mesurée n'est pas reprise"
    );
    assert_eq!(
        tune_core::audio::replaygain::bibliotheque::compter_la_bibliotheque_replaygain(&db)
            .map(|c| (c.analysees, c.eligibles)),
        Some((2, 2)),
        "la jauge de bibliothèque (#5648) compte les deux pistes"
    );

    // --- 5. Second redémarrage, pour la réanalyse aussi. ---
    drop(db);
    drop(etat);
    let etat = demarrer(&b);
    let db = etat.backend.clone();
    scan_de_demarrage(&db).await;
    verifier("second redémarrage", &db, &b, id);
    tune_core::audio::replaygain::analyze_track_batch(&db).await;
    verifier("réanalyse après le second redémarrage", &db, &b, id);
}
