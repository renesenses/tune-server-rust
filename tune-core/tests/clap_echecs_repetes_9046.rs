//! tune-server-rust#9046 (fil 2907) — l'analyse acoustique affichée « en
//! cours » sans avancer.
//!
//! Piste (b) : une piste dont l'inférence PANIQUE n'était jamais estampillée.
//! La requête de candidats (`LIMIT 32`, sans `ORDER BY`) la rendait à chaque
//! lot : `Lot(0)`, sieste, et on recommence, sans fin. Après trois échecs de
//! suite, la piste est désormais mise « à réessayer plus tard »
//! (`audio_embed_retry_later`) et sort du balayage.
//!
//! Témoin d'activité : il ne passait à vrai qu'à la FIN du premier lot
//! productif ; « Ce qui tourne » disait « rien ne tourne » pendant ce lot.
//!
//! Ces témoins n'ont ni le modèle ni onnxruntime : l'inférence est un faux.
//! Les états observés sont des globaux du processus : témoins sérialisés.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tune_core::audio::embedding::{
    Inference, TourDeBalayage, balayage_acoustique_en_cours, balayage_au_repos_pour_les_essais,
    reprendre_apres_arret_pour_les_essais, tour_de_balayage,
};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::priorite::oublier_la_lecture_pour_les_essais;

static SERIE: Mutex<()> = Mutex::new(());

/// Une seconde de WAV 48 kHz / 16 bits / mono.
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

/// Une base migrée avec les pistes `ids`, décodables, sans pause entre fichiers.
fn base(ids: &[i64]) -> (tempfile::TempDir, Arc<dyn DbBackend>) {
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
    for id in ids {
        let p = dir.path().join(format!("{id}.wav"));
        ecrire_wav(&p);
        let chemin = p.to_string_lossy().to_string();
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format) \
             VALUES (?, 'T', 1, 1, ?, 'wav')",
            &[id, &chemin],
        )
        .unwrap();
    }
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    SettingsRepo::with_backend(backend.clone())
        .set("audio_embedding_throttle", "rapide")
        .unwrap();
    (dir, backend)
}

/// Un faux modèle qui panique à chaque inférence.
struct ModeleQuiPanique;
impl Inference for ModeleQuiPanique {
    fn inferer(&mut self, _wav: &[f32]) -> Result<Vec<f32>, String> {
        panic!("inférence en échec (faux modèle, #9046)");
    }
}

/// Un faux modèle qui relève le témoin d'activité PENDANT le lot.
struct ModeleTemoin(Arc<AtomicBool>);
impl Inference for ModeleTemoin {
    fn inferer(&mut self, _wav: &[f32]) -> Result<Vec<f32>, String> {
        if balayage_acoustique_en_cours() {
            self.0.store(true, Ordering::SeqCst);
        }
        Ok(vec![(1.0f32 / 512.0).sqrt(); 512])
    }
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
    balayage_au_repos_pour_les_essais();
    serie
}

fn cle(backend: &Arc<dyn DbBackend>, id: i64, k: &str) -> Option<String> {
    backend
        .query_one(
            "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
            &[&id, &k],
        )
        .ok()
        .flatten()
        .and_then(|c| c.first().and_then(|v| v.as_string()))
}

#[test]
fn une_piste_en_echec_repete_est_mise_a_reessayer_plus_tard() {
    let _serie = preparer();
    let ids = [904_601_i64, 904_602];
    let (_dir, backend) = base(&ids);
    let rt = executeur();
    let mut session: Option<Arc<Mutex<ModeleQuiPanique>>> = None;
    let mut tours = Vec::new();
    for _ in 0..4 {
        tours.push(
            rt.block_on(tour_de_balayage(&backend, &mut session, || async {
                Some(ModeleQuiPanique)
            })),
        );
    }
    println!("tours = {tours:?}");
    assert_eq!(
        &tours[..2],
        &[TourDeBalayage::Lot(0), TourDeBalayage::Lot(0)],
        "deux premiers échecs : la piste est réessayée"
    );
    assert_eq!(
        tours[2],
        TourDeBalayage::Lot(2),
        "troisième échec : les deux pistes sont reportées, et c'est un progrès"
    );
    assert_eq!(
        tours[3],
        TourDeBalayage::RienAAnalyser,
        "sans le report, le même lot revenait à l'infini (#9046, piste b)"
    );
    for id in ids {
        assert!(
            cle(&backend, id, "audio_embed_retry_later").is_some(),
            "piste {id} : témoin « à réessayer plus tard » absent"
        );
        assert_eq!(
            cle(&backend, id, "audio_embed_analyzed"),
            None,
            "piste {id} : reportée, PAS marquée analysée"
        );
    }
    // Tant qu'elles sont reportées, elles sont COMPTÉES comme telles : la
    // route en tire `waiting_reason`, la jauge ne reste pas figée sans un mot.
    assert_eq!(
        tune_core::audio::embedding_store::deferred_count(&backend, &seuil()),
        2,
        "pistes à réessayer plus tard non comptées dans les reports"
    );

    // Deux reports périmés de plus : au troisième, la piste est abandonnée
    // (témoin d'analyse sans empreinte, compté en échec), et ne revient plus.
    for report in 2..=3 {
        perimer_les_reports(&backend);
        let mut suite = Vec::new();
        for _ in 0..3 {
            suite.push(
                rt.block_on(tour_de_balayage(&backend, &mut session, || async {
                    Some(ModeleQuiPanique)
                })),
            );
        }
        println!("report {report} : {suite:?}");
        assert_eq!(suite[2], TourDeBalayage::Lot(2), "report {report}");
    }
    let fin = rt.block_on(tour_de_balayage(&backend, &mut session, || async {
        Some(ModeleQuiPanique)
    }));
    assert_eq!(
        fin,
        TourDeBalayage::RienAAnalyser,
        "trois reports : la piste ne revient plus toutes les six heures"
    );
    for id in ids {
        assert!(
            cle(&backend, id, "audio_embed_analyzed").is_some(),
            "piste {id} : abandonnée, donc marquée traitée (comptée en échec)"
        );
        assert_eq!(cle(&backend, id, "audio_embed_retry_later"), None);
    }
    assert_eq!(
        tune_core::audio::embedding_store::deferred_count(&backend, &seuil()),
        0
    );
}

/// Le seuil de report pour l'instant présent.
fn seuil() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    tune_core::library::local_path::deferral_threshold(now)
}

/// Faire comme si six heures avaient passé : les reports sont périmés.
fn perimer_les_reports(backend: &Arc<dyn DbBackend>) {
    let ancien = tune_core::library::local_path::deferral_stamp(1);
    backend
        .execute(
            "UPDATE track_metadata SET value = ? WHERE key = 'audio_embed_retry_later'",
            &[&ancien.as_str()],
        )
        .unwrap();
}

#[test]
fn le_temoin_d_activite_est_vrai_des_le_debut_du_lot() {
    let _serie = preparer();
    let (_dir, backend) = base(&[904_611]);
    let rt = executeur();
    let vu = Arc::new(AtomicBool::new(false));
    let mut session: Option<Arc<Mutex<ModeleTemoin>>> = None;
    let v = vu.clone();
    let tour = rt.block_on(tour_de_balayage(
        &backend,
        &mut session,
        move || async move { Some(ModeleTemoin(v)) },
    ));
    assert_eq!(tour, TourDeBalayage::Lot(1));
    assert!(
        vu.load(Ordering::SeqCst),
        "pendant le premier lot, le témoin d'activité doit dire « en cours » (#9046)"
    );
}
