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
    assert!(
        spawn_library_scan_avec_lecteur(
            state.clone(),
            false,
            None,
            None,
            lecteur,
            DELAI_LECTURE_CREDITS
        )
        .await
    );
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

/// Un lecteur factice qui ne rend JAMAIS la main tant que le témoin ne le
/// libère. La libération passe par `Drop` : même un témoin qui rougit relâche
/// ses fils, et le scan qu'il a lancé finit par rendre le droit de scanner.
struct Muet(Arc<std::sync::atomic::AtomicBool>);

impl Muet {
    fn new() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }
    /// Relâché de toute façon au bout de `d` : un témoin unitaire qui rougit
    /// parce que la lecture n'est plus bornée rend quand même la main.
    fn liberer_apres(d: Duration) -> Self {
        let muet = Self::new();
        let libere = muet.0.clone();
        std::thread::spawn(move || {
            std::thread::sleep(d);
            libere.store(true, Ordering::SeqCst);
        });
        muet
    }
    fn attendre(libere: &std::sync::atomic::AtomicBool) {
        while !libere.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Muet {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_fichier_dont_la_lecture_ne_rend_jamais_la_main_ne_fige_pas_le_lot_5202() {
    let _seul = serialiser_les_scans_de_test_sans_bloquer().await;
    let (_dir, state, _) = fixture();
    let muet = Muet::new();
    let libere = muet.0.clone();
    let lecteur: LecteurMetadonnees = Arc::new(move |p: &std::path::Path| {
        // Le fichier du milieu : un partage SMB qui ne répond plus sur lui.
        if p.ends_with("piste-1.wav") {
            Muet::attendre(&libere);
        }
        HashMap::from([("composer".into(), "Temoin 5202".into())])
    });
    let mut rx = state.event_bus.subscribe();
    assert!(
        spawn_library_scan_avec_lecteur(
            state.clone(),
            false,
            None,
            None,
            lecteur,
            Duration::from_millis(300)
        )
        .await
    );
    let fin = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = rx.recv().await.unwrap();
            if event.event_type == "library.scan.completed" {
                return event.data;
            }
        }
    })
    .await
    .expect("#5202 : un fichier dont la lecture ne rend jamais la main fige le lot entier");
    attendre_que_le_droit_de_scanner_soit_libre().await;
    assert_eq!(
        fin["inserted"], 3,
        "les trois pistes sont importées : {fin}"
    );
    let credits = state
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
        credits, 2,
        "seul le fichier muet est importé sans ses crédits ; les autres gardent les leurs"
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
    drop(muet);
}

#[test]
fn un_stockage_muet_n_est_plus_relu_apres_trois_expirations_de_suite_5202() {
    let muet = Muet::liberer_apres(Duration::from_secs(15));
    let libere = muet.0.clone();
    let tentatives = Arc::new(AtomicUsize::new(0));
    let n = tentatives.clone();
    let lecteur: LecteurMetadonnees = Arc::new(move |_: &std::path::Path| {
        n.fetch_add(1, Ordering::SeqCst);
        Muet::attendre(&libere);
        HashMap::new()
    });
    let chemins: Vec<String> = (0..10).map(|i| format!("/partage/muet/{i}.flac")).collect();
    let bus = tune_core::event_bus::EventBus::new();
    let lu = lire_metadonnees_du_lot(
        &chemins,
        &lecteur,
        Duration::from_millis(50),
        || false,
        &bus,
        0,
        0,
        10,
    );
    assert_eq!(
        lu.map(|m| m.len()),
        Some(0),
        "le lot continue, sans crédits pour les fichiers muets"
    );
    assert_eq!(
        tentatives.load(Ordering::SeqCst),
        import_progress::EXPIRATIONS_AVANT_ABANDON,
        "#5202 : un stockage qui ne répond plus doit être abandonné après trois expirations, \
         pas attendu fichier après fichier"
    );
    drop(muet);
}

#[test]
fn arreter_pendant_une_lecture_bloquee_rend_la_main_sans_attendre_le_delai_5202() {
    let muet = Muet::liberer_apres(Duration::from_secs(15));
    let libere = muet.0.clone();
    let lecteur: LecteurMetadonnees = Arc::new(move |_: &std::path::Path| {
        Muet::attendre(&libere);
        HashMap::new()
    });
    let arret = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a = arret.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        a.store(true, Ordering::SeqCst);
    });
    let bus = tune_core::event_bus::EventBus::new();
    let debut = std::time::Instant::now();
    let lu = lire_metadonnees_du_lot(
        &["/partage/muet/0.flac".to_string()],
        &lecteur,
        Duration::from_secs(60),
        || arret.load(Ordering::SeqCst),
        &bus,
        0,
        0,
        1,
    );
    let ecoule = debut.elapsed();
    assert!(lu.is_none(), "un lot arrêté n'ouvre pas sa transaction");
    assert!(
        ecoule < Duration::from_secs(10),
        "#5202 : Arrêter doit agir pendant une lecture bloquée, pas après son délai ({ecoule:?})"
    );
    drop(muet);
}
