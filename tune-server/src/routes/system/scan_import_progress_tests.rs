//! Le vrai scan manuel, des WAV et une base SQLite de fichier (#5202).
use super::*;
use std::collections::HashMap;
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;
use tune_core::db::track_repo::TrackRepo;

fn fixture() -> (
    tune_core::test_scratch::ScratchDir,
    AppState,
    std::path::PathBuf,
) {
    let dir =
        tune_core::test_scratch::scratch_dir_in(std::env::current_dir().unwrap(), "scan-5202");
    let musique = dir.join("musique");
    std::fs::create_dir_all(&musique).unwrap();
    for i in 0..3u8 {
        let data = vec![i; 2048];
        let mut wav = b"RIFF".to_vec();
        wav.extend((36u32 + data.len() as u32).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(44100u32.to_le_bytes());
        wav.extend(88200u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((data.len() as u32).to_le_bytes());
        wav.extend(data);
        std::fs::write(musique.join(format!("piste-{i}.wav")), wav).unwrap();
    }
    let state =
        AppState::new(dir.join("tune.db").to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_str().unwrap()]).unwrap(),
        )
        .unwrap();
    settings.set("enrich_on_scan", "false").unwrap();
    (dir, state, musique)
}

async fn lancer(state: &AppState, lecteur: LecteurMetadonnees) -> Vec<Value> {
    let mut rx = state.event_bus.subscribe();
    assert!(spawn_library_scan_avec_lecteur(state.clone(), false, None, None, lecteur).await);
    let mut progression = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(90), rx.recv())
            .await
            .expect("le scan #5202 doit rendre la main")
            .unwrap();
        if event.event_type == "library.scan.progress" {
            progression.push(event.data);
        } else if event.event_type == "library.scan.completed" {
            attendre_que_le_droit_de_scanner_soit_libre().await;
            return progression;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_lectures_de_credits_ne_tiennent_pas_la_transaction_sqlite_5202() {
    let _seul = serialiser_les_scans_de_test_sans_bloquer().await;
    let (_dir, state, _) = fixture();
    let lectures = Arc::new(AtomicUsize::new(0));
    let sous_transaction = Arc::new(AtomicUsize::new(0));
    let db = state.db.as_ref().unwrap().clone();
    let n = lectures.clone();
    let tx = sous_transaction.clone();
    let progression = lancer(
        &state,
        Arc::new(move |_| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                // Un accès SMB lent : la progression suivante doit dire qu'un
                // fichier est lu, tout en laissant les compteurs d'import à zéro.
                std::thread::sleep(Duration::from_millis(2100));
            }
            if !db.connection().lock().unwrap().is_autocommit() {
                tx.fetch_add(1, Ordering::SeqCst);
            }
            HashMap::from([("composer".into(), "Temoin 5202".into())])
        }),
    )
    .await;
    assert_eq!(
        lectures.load(Ordering::SeqCst),
        3,
        "les trois fichiers ont été relus"
    );
    assert_eq!(
        sous_transaction.load(Ordering::SeqCst),
        0,
        "#5202 : la lecture réseau des crédits retient la transaction SQLite"
    );
    let n = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'composer' AND value = 'Temoin 5202'",
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    assert_eq!(
        n, 3,
        "les crédits préchargés doivent rejoindre les nouvelles pistes"
    );
    assert!(
        progression.iter().any(|p| p["stage"] == "extended_metadata"
            && p["metadata_total"] == 3
            && p["scanned"] == 0
            && p["current_file"].is_string()),
        "le lot doit annoncer son travail avant d'importer, sans inventer des pistes validées"
    );
    assert!(
        progression.iter().any(|p| p["stage"] == "extended_metadata"
            && p["metadata_read"] == 1
            && p["scanned"] == 0),
        "la relecture lente doit publier son avancement avant la fin du lot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arreter_pendant_les_credits_ne_lit_pas_le_reste_du_lot_et_ne_purge_pas_5202() {
    let _seul = serialiser_les_scans_de_test_sans_bloquer().await;
    let (_dir, state, musique) = fixture();
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut ancienne = tune_core::db::models::Track::new("ancienne".into());
    ancienne.file_path = Some(musique.join("absente.wav").to_string_lossy().into_owned());
    let id = repo.create(&ancienne).unwrap();
    let lectures = Arc::new(AtomicUsize::new(0));
    let n = lectures.clone();
    lancer(
        &state,
        Arc::new(move |_| {
            n.fetch_add(1, Ordering::SeqCst);
            SCAN_GATE.request_cancel();
            HashMap::new()
        }),
    )
    .await;
    assert_eq!(
        lectures.load(Ordering::SeqCst),
        1,
        "#5202 : Arrêter doit interrompre les crédits entre deux fichiers, pas après le lot entier"
    );
    let ids = state
        .backend
        .query_many("SELECT id FROM tracks", &[])
        .unwrap();
    assert_eq!(
        ids.len(),
        1,
        "un lot annulé avant import n'ajoute aucune piste"
    );
    assert_eq!(
        ids[0][0].as_i64(),
        Some(id),
        "l'arrêt ne doit pas purger la piste absente"
    );
    let rapport: Value = serde_json::from_str(
        &SettingsRepo::with_backend(state.backend.clone())
            .get("scan_result")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        rapport["cancelled"], true,
        "le bilan final doit rester annulé"
    );
    assert_eq!(rapport["auto_enrichment"]["started"], false);
    assert_eq!(
        rapport["auto_enrichment"]["skipped_reason"],
        "scan_cancelled"
    );
    assert!(
        state
            .db
            .as_ref()
            .unwrap()
            .connection()
            .lock()
            .unwrap()
            .is_autocommit()
    );
}
