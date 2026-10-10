//! Fuite mémoire de la passe acoustique (CLAP), relevée sur le .18 le 08/10 :
//! 12,2 Go de RSS en trois jours.
//!
//! Une fois la bibliothèque analysée, chaque fin de pause (lecture, chaleur…)
//! rechargeait le modèle CLAP (287 Mo, session ORT) pour un lot qui ne trouvait
//! RIEN à analyser (`embedded=0`), puis gardait la session pendant la sieste de
//! quinze minutes. 23 chargements en trois jours, ~235 Mo nets retenus par
//! cycle dans les arènes glibc des fils `spawn_blocking`.
//!
//! Ces témoins n'ont ni le modèle ni onnxruntime : le chargement est un faux
//! qui COMPTE ses appels. C'est le nombre de chargements qu'on garde, pas ce
//! qu'ORT en fait.
//!
//! Les états observés sont des globaux du processus (témoin de lecture, demande
//! d'arrêt) : les témoins sont sérialisés.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tune_core::audio::embedding::{
    Inference, TourDeBalayage, reprendre_apres_arret_pour_les_essais, tour_de_balayage,
};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::priorite::oublier_la_lecture_pour_les_essais;

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

/// Une base migrée avec `n` pistes locales décodables, passe réglée sans pause
/// entre fichiers (`rapide`).
fn base(n: i64) -> (tempfile::TempDir, Arc<dyn DbBackend>) {
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
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    SettingsRepo::with_backend(backend.clone())
        .set("audio_embedding_throttle", "rapide")
        .unwrap();
    (dir, backend)
}

/// Un faux modèle : rend un vecteur normé, sans rien calculer.
struct FauxModele;

impl Inference for FauxModele {
    fn inferer(&mut self, _wav: &[f32]) -> Result<Vec<f32>, String> {
        Ok(vec![(1.0f32 / 512.0).sqrt(); 512])
    }
}

type Session = Option<Arc<Mutex<FauxModele>>>;

/// Un tour de balayage dont le chargement du modèle est compté.
fn un_tour(
    rt: &tokio::runtime::Runtime,
    backend: &Arc<dyn DbBackend>,
    session: &mut Session,
    chargements: &Arc<AtomicUsize>,
) -> TourDeBalayage {
    let compte = chargements.clone();
    rt.block_on(tour_de_balayage(backend, session, move || async move {
        compte.fetch_add(1, Ordering::SeqCst);
        Some(FauxModele)
    }))
}

fn executeur() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn preparer() -> std::sync::MutexGuard<'static, ()> {
    let serie = SERIE.lock().unwrap_or_else(|e| e.into_inner());
    oublier_la_lecture_pour_les_essais();
    reprendre_apres_arret_pour_les_essais();
    serie
}

/// LE témoin de la fuite : bibliothèque sans rien à analyser, session relâchée
/// (comme après une pause). Le tour ne doit PAS charger le modèle.
#[test]
fn rien_a_analyser_le_modele_n_est_pas_charge() {
    let _serie = preparer();
    let (_dir, backend) = base(0);
    let rt = executeur();
    let chargements = Arc::new(AtomicUsize::new(0));
    let mut session: Session = None;

    let tour = un_tour(&rt, &backend, &mut session, &chargements);

    println!(
        "rien_a_analyser : chargements={} tour={tour:?} session_chargee={}",
        chargements.load(Ordering::SeqCst),
        session.is_some()
    );
    assert_eq!(
        chargements.load(Ordering::SeqCst),
        0,
        "aucune piste à analyser : charger le modèle CLAP (287 Mo) ne sert à rien \
         — c'est ce rechargement, 23 fois en trois jours, qui a fait monter le .18 \
         à 12,2 Go"
    );
    assert_eq!(tour, TourDeBalayage::RienAAnalyser);
    assert!(session.is_none());
}

/// Le scénario du .18 : la bibliothèque est analysée, une pause relâche la
/// session, la pause se lève. Le tour suivant ne doit pas recharger.
#[test]
fn apres_une_pause_une_bibliotheque_analysee_ne_recharge_pas_le_modele() {
    let _serie = preparer();
    let (_dir, backend) = base(2);
    let rt = executeur();
    let chargements = Arc::new(AtomicUsize::new(0));
    let mut session: Session = None;

    let premier = un_tour(&rt, &backend, &mut session, &chargements);
    assert_eq!(
        premier,
        TourDeBalayage::Lot(2),
        "les deux pistes sont analysées"
    );
    assert_eq!(chargements.load(Ordering::SeqCst), 1);

    // Une pause (lecture, chaleur…) relâche la session : `entrer_en_pause`.
    session = None;
    let apres_pause = un_tour(&rt, &backend, &mut session, &chargements);

    println!(
        "apres_pause : chargements={} tour={apres_pause:?} session_chargee={}",
        chargements.load(Ordering::SeqCst),
        session.is_some()
    );
    assert_eq!(
        chargements.load(Ordering::SeqCst),
        1,
        "la bibliothèque est déjà analysée : la fin de la pause ne doit pas \
         recharger le modèle"
    );
    assert!(session.is_none());
}

/// La fin du balayage relâche la session : elle ne passe pas les quinze minutes
/// de sieste en mémoire.
#[test]
fn la_fin_du_balayage_relache_la_session() {
    let _serie = preparer();
    let (_dir, backend) = base(2);
    let rt = executeur();
    let chargements = Arc::new(AtomicUsize::new(0));
    let mut session: Session = None;

    assert_eq!(
        un_tour(&rt, &backend, &mut session, &chargements),
        TourDeBalayage::Lot(2)
    );
    assert!(session.is_some(), "il restait du travail : la session sert");

    let fin = un_tour(&rt, &backend, &mut session, &chargements);

    println!(
        "fin_du_balayage : chargements={} tour={fin:?} session_chargee={}",
        chargements.load(Ordering::SeqCst),
        session.is_some()
    );
    assert!(
        session.is_none(),
        "plus rien à analyser : la session ORT doit être relâchée avant la sieste \
         de quinze minutes"
    );
    assert_eq!(fin, TourDeBalayage::RienAAnalyser);
}

/// Témoin de non-régression : s'il reste des pistes, le modèle est chargé, une
/// fois, et le lot tourne.
#[test]
fn il_reste_des_pistes_le_modele_est_charge_une_fois() {
    let _serie = preparer();
    let (_dir, backend) = base(3);
    let rt = executeur();
    let chargements = Arc::new(AtomicUsize::new(0));
    let mut session: Session = None;

    let tour = un_tour(&rt, &backend, &mut session, &chargements);

    assert_eq!(chargements.load(Ordering::SeqCst), 1);
    assert_eq!(tour, TourDeBalayage::Lot(3));
}
