//! #5138 — la passe acoustique (CLAP) ne doit plus tenir l'exécuteur tokio.
//!
//! JeromeQ (Linux, 8 cœurs, 34 091 pistes) : gels de l'exécuteur, lecture
//! hachée, arrêt impossible, à partir du moment où l'analyse acoustique reprend
//! pendant la lecture. L'inférence ONNX tournait À MÊME la tâche async : chaque
//! piste tenait un ouvrier de l'exécuteur le temps d'une inférence.
//!
//! Ces témoins n'ont ni le modèle (287 Mo) ni onnxruntime : un faux modèle qui
//! DORT le temps d'une inférence tient le fil exactement comme le vrai. Les
//! mesures sur le vrai modèle sont dans le banc `embedding::banc_5138`
//! (ignoré, Linux).
//!
//! Les états observés sont des globaux du processus (témoin de lecture, demande
//! d'arrêt) : les témoins sont sérialisés.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tune_core::audio::embedding::{
    Inference, analyze_embedding_batch, arreter, reprendre_apres_arret_pour_les_essais,
};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::taches_de_fond::priorite::{
    noter_etat_de_lecture, oublier_la_lecture_pour_les_essais,
};

static SERIE: Mutex<()> = Mutex::new(());

/// Une seconde de WAV 48 kHz / 16 bits / mono : rien à rééchantillonner.
fn ecrire_wav(path: &std::path::Path) {
    const HZ: u32 = 48_000;
    let mut donnees = Vec::with_capacity(HZ as usize * 2);
    for i in 0..HZ {
        let g = (((i % 200) as i32 - 100) * 100) as i16;
        donnees.extend_from_slice(&g.to_le_bytes());
    }
    let mut w = Vec::with_capacity(donnees.len() + 44);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&HZ.to_le_bytes());
    w.extend_from_slice(&(HZ * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
    w.extend_from_slice(&donnees);
    std::fs::write(path, w).unwrap();
}

/// Une base migrée avec `n` pistes locales décodables, et la passe réglée sans
/// pause entre fichiers (`rapide`) pour que les témoins restent courts.
fn base(n: i64) -> (tempfile::TempDir, SqliteDb, Arc<dyn DbBackend>) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
        &[],
    )
    .unwrap();
    for id in 1..=n {
        let p = dir.path().join(format!("{id}.wav"));
        ecrire_wav(&p);
        let chemin = p.to_string_lossy().to_string();
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format) \
             VALUES (?, 'T', 1, 1, ?, 'wav')",
            &[&id, &chemin],
        )
        .unwrap();
    }
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    SettingsRepo::with_backend(backend.clone())
        .set("audio_embedding_throttle", "rapide")
        .unwrap();
    (dir, db, backend)
}

/// Un faux modèle : il tient son fil `duree`, appelle `pendant` avec le rang de
/// l'appel, et rend un vecteur normé.
struct FauxModele {
    duree: Duration,
    appels: Arc<AtomicUsize>,
    pendant: fn(usize),
}

impl Inference for FauxModele {
    fn inferer(&mut self, _wav: &[f32]) -> Result<Vec<f32>, String> {
        let rang = self.appels.fetch_add(1, Ordering::SeqCst) + 1;
        (self.pendant)(rang);
        // `sleep` et non une boucle de calcul : c'est le fil qui est tenu,
        // c'est ce qu'on mesure, et le témoin ne chauffe pas la CI.
        std::thread::sleep(self.duree);
        Ok(vec![(1.0f32 / 512.0).sqrt(); 512])
    }
}

fn faux(duree: Duration, pendant: fn(usize)) -> (Arc<Mutex<FauxModele>>, Arc<AtomicUsize>) {
    let appels = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(Mutex::new(FauxModele {
            duree,
            appels: appels.clone(),
            pendant,
        })),
        appels,
    )
}

/// Un exécuteur à deux ouvriers. Construit à la main plutôt que par
/// `#[tokio::test]` : la garde de [`SERIE`] ne doit pas traverser un `.await`.
fn executeur() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn analysee(db: &SqliteDb, id: i64) -> bool {
    TrackMetadataRepo::new(db.clone())
        .get_all(id)
        .unwrap()
        .contains_key("audio_embed_analyzed")
}

/// LE témoin de #5138 : pendant une inférence, l'exécuteur doit continuer de
/// servir le reste. Exécuteur à UN ouvrier, comme sur un Pi ou un NAS — et
/// comme sur toute machine dès que l'ouvrier qui porte la passe porte aussi la
/// tâche qui attend : la sonde dort 5 ms en boucle et note son pire retard.
///
/// Avant le correctif, le retard égale la durée d'une inférence (1 s ici). La
/// marge est large pour la CI : 500 ms.
#[test]
fn une_inference_ne_tient_pas_l_executeur_5138() {
    let _serie = SERIE.lock().unwrap_or_else(|e| e.into_inner());
    oublier_la_lecture_pour_les_essais();
    reprendre_apres_arret_pour_les_essais();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (_dir, db, backend) = base(3);
    let (modele, appels) = faux(Duration::from_millis(1000), |_| {});

    let pire = rt.block_on(async move {
        let fini = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sonde = {
            let fini = fini.clone();
            tokio::spawn(async move {
                let mut pire = Duration::ZERO;
                while !fini.load(Ordering::SeqCst) {
                    let t = Instant::now();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    pire = pire.max(t.elapsed().saturating_sub(Duration::from_millis(5)));
                }
                pire
            })
        };
        // Sur un OUVRIER, comme `embedding::spawn` en production — pas sur le
        // fil de `block_on`, qui n'en est pas un.
        let lot = tokio::spawn(async move { analyze_embedding_batch(&backend, &modele).await });
        let n = lot.await.unwrap();
        fini.store(true, Ordering::SeqCst);
        assert_eq!(n, 3, "les trois pistes doivent être traitées");
        sonde.await.unwrap()
    });

    assert_eq!(appels.load(Ordering::SeqCst), 3);
    assert!(analysee(&db, 3));
    assert!(
        pire < Duration::from_millis(500),
        "l'exécuteur est resté bloqué {pire:?} pendant une inférence d'1 s : \
         l'inférence CLAP tourne sur un ouvrier de l'exécuteur au lieu du vivier \
         bloquant (#5138)"
    );
}

/// Une zone se met à jouer PENDANT l'inférence de la première piste : le lot
/// s'arrête à la frontière de piste, sans en commencer une deuxième. Le témoin
/// de lecture est celui en MÉMOIRE (`priorite`) : la table `zones` ne dit rien
/// ici, comme lorsque la requête qui la lit attend une connexion libre (8,4 s
/// chez JeromeQ).
#[test]
fn la_lecture_arrete_le_lot_a_la_frontiere_de_piste_5138() {
    let _serie = SERIE.lock().unwrap_or_else(|e| e.into_inner());
    oublier_la_lecture_pour_les_essais();
    reprendre_apres_arret_pour_les_essais();

    let (_dir, db, backend) = base(4);
    let (modele, appels) = faux(Duration::from_millis(50), |rang| {
        if rang == 1 {
            noter_etat_de_lecture(99, "playing");
        }
    });
    let n = executeur().block_on(async { analyze_embedding_batch(&backend, &modele).await });
    oublier_la_lecture_pour_les_essais();

    assert_eq!(
        appels.load(Ordering::SeqCst),
        1,
        "une zone joue depuis la première piste : aucune inférence de plus ne \
         doit partir (#5138)"
    );
    assert_eq!(n, 1, "la piste commencée est finie et écrite");
    assert!(analysee(&db, 1));
    assert!(!analysee(&db, 2));
}

/// L'arrêt du serveur tombe PENDANT une inférence : le lot rend la main sans
/// commencer la piste suivante, et la piste interrompue n'est PAS marquée
/// analysée — elle n'a rien d'illisible, elle sera reprise au démarrage.
#[test]
fn l_arret_du_serveur_arrete_le_lot_en_cours_5138() {
    let _serie = SERIE.lock().unwrap_or_else(|e| e.into_inner());
    oublier_la_lecture_pour_les_essais();
    reprendre_apres_arret_pour_les_essais();

    let (_dir, db, backend) = base(4);
    let (modele, appels) = faux(Duration::from_millis(50), |rang| {
        if rang == 1 {
            arreter();
        }
    });
    let debut = Instant::now();
    let n = executeur().block_on(async { analyze_embedding_batch(&backend, &modele).await });
    let duree = debut.elapsed();
    let arret_vu = tune_core::audio::embedding::arret_demande();
    reprendre_apres_arret_pour_les_essais();

    assert!(arret_vu);
    assert_eq!(
        appels.load(Ordering::SeqCst),
        1,
        "l'arrêt est demandé : aucune inférence de plus ne doit partir (#5138)"
    );
    assert_eq!(n, 0, "rien n'est compté comme traité après l'arrêt");
    assert!(
        !analysee(&db, 1),
        "la piste interrompue par l'arrêt ne doit pas être marquée analysée"
    );
    assert!(duree < Duration::from_secs(5), "{duree:?}");
}
