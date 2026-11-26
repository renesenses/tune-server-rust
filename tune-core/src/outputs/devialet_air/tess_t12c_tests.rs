//! TESS T12c — Devialet AIR sous la chaîne TESS, vérifié sur ce que l'AMPLI
//! reçoit : la vraie porte de lecture (`PlaybackOrchestrator::play`), la vraie
//! sortie face au simulateur d'ampli, les URL servies par le faux serveur
//! `/stream` (`orchestrator::faux_serveur_de_flux_tess`).
//!
//! - crossfeed : la voie droite muette de la source s'allume dans le PCM
//!   reçu par l'ampli ;
//! - Pure, crossfeed coché : la source à l'échantillon près ;
//! - la GARDE DE VOLUME (#8920) est inchangée : avec ou sans chaîne, l'ampli
//!   reçoit exactement les mêmes consignes de volume, jamais au-dessus de la
//!   sécurité.
//!
//! Aucun son, rien hors de `127.0.0.1`.

use std::sync::Arc;
use std::time::Duration;

use super::super::protocole::opcode;
use super::super::reglages::BornesDb;
use super::super::simulateur::{Simulateur, annonce_de_test};
use super::pieces_du_banc_securite;
use crate::audio::volume_limites::cle_securite;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::faux_serveur_de_flux_tess as banc;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::traits::OutputTarget;

const CADENCE: u32 = 44_100;
const TRAMES: usize = 26_460;
/// −45 dB sur la course par défaut (−80 … −10 dB) : la sécurité des bancs.
const SECURITE: f64 = 0.5;
const SECURITE_DB: f64 = -45.0;

#[derive(Clone, Copy, Debug)]
enum Chaine {
    Aucune,
    Crossfeed,
    CrossfeedSousPure,
}

struct Lecture {
    pcm: Vec<[i32; 2]>,
    source: Vec<[i16; 2]>,
    volumes: Vec<f32>,
}

/// Une lecture de bout en bout sur l'ampli simulé, réglé à `ampli_db`.
async fn jouer_sur_air(chaine: Chaine, ampli_db: f64) -> Lecture {
    let mut annonce = annonce_de_test();
    annonce.volume_demi_db = (ampli_db * 2.0) as i16;
    let (sim, sortie, _drapeau, relais): (Simulateur, _, _, _) =
        pieces_du_banc_securite(annonce, true, BornesDb::PAR_DEFAUT, Some(SECURITE_DB)).await;
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        banc::demarrer().await,
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.advertised_ip = Some("127.0.0.1".into());
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));
    let did = sortie.device_id().to_string();
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Expert T12c", Some("devialet_air"), Some(&did))
        .unwrap();
    let v = BornesDb::PAR_DEFAUT.db_vers_volume(ampli_db);
    ZoneRepo::with_backend(db.clone())
        .update_volume(zone_id, v * 100.0)
        .unwrap();
    orch.playback.set_volume(zone_id, v).await;
    SettingsRepo::with_backend(db.clone())
        .set(&cle_securite(zone_id), &SECURITE.to_string())
        .unwrap();
    orch.outputs.lock().await.register(Box::new(sortie));

    let (wav, source) = banc::wav_gauche_seule("t12c-air", CADENCE, TRAMES);
    let duree_ms = (TRAMES as i64 * 1_000) / i64::from(CADENCE);
    let track_id = banc::inscrire_la_piste(&db, wav.as_str().as_ref(), CADENCE, duree_ms);
    match chaine {
        Chaine::Aucune => {}
        Chaine::Crossfeed => banc::armer_le_crossfeed(&orch, zone_id),
        Chaine::CrossfeedSousPure => {
            banc::armer_le_crossfeed(&orch, zone_id);
            banc::armer_pure(&orch, zone_id);
        }
    }
    let r = orch
        .play(PlayRequest {
            zone_id,
            output_device_id: Some(did.clone()),
            track_id: Some(track_id),
            source: Some("local".into()),
            duration_ms: Some(duree_ms),
            media_format: Some("wav".into()),
            sample_rate: Some(CADENCE),
            bit_depth: Some(16),
            ..Default::default()
        })
        .await;
    assert!(r.is_ok(), "la lecture doit partir : {:?}", r.err());

    let recus = |sim: &Simulateur| -> usize { sim.etat().paquets.values().map(|p| p.len()).sum() };
    let mut vu = 0usize;
    let mut calme = 0u32;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = recus(&sim);
        if n > 0 && n == vu {
            calme += 1;
            if calme >= 15 {
                break;
            }
        } else {
            calme = 0;
        }
        vu = n;
    }
    let pcm = {
        let e = sim.etat();
        let flux = e
            .paquets
            .iter()
            .max_by_key(|(_, p)| p.len())
            .map(|(f, _)| *f)
            .expect("l'ampli n'a reçu aucun paquet");
        e.pcm(flux).expect("un trou dans le flux reçu par l'ampli")
    };
    let volumes = sim
        .etat()
        .commandes
        .iter()
        .filter(|c| c.opcode == opcode::VOLUME)
        .map(|c| c.valeur)
        .collect();
    if let Some(s) = { orch.outputs.lock().await.get(&did) } {
        let _ = s.lock().await.stop().await;
    }
    relais.abort();
    Lecture {
        pcm,
        source,
        volumes,
    }
}

/// Crossfeed sur Devialet AIR : la voie droite muette de la source s'allume
/// dans le PCM que reçoit l'ampli.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossfeed_sur_air_allume_la_voie_droite_t12c() {
    let l = jouer_sur_air(Chaine::Crossfeed, -50.0).await;
    let c = banc::constater(&l.pcm, 24, &l.source, 1.0);
    println!("T12c crossfeed Devialet AIR : {c:?}");
    assert!(
        c.droite_max > 500,
        "T12c : le crossfeed doit allumer la voie droite reçue par l'ampli ({c:?})"
    );
}

/// Pure, crossfeed coché : l'ampli reçoit la source à l'échantillon près.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pure_sur_air_rien_n_est_traite_t12c() {
    let l = jouer_sur_air(Chaine::CrossfeedSousPure, -50.0).await;
    let c = banc::constater(&l.pcm, 24, &l.source, 1.0);
    println!("T12c Pure Devialet AIR : {c:?}");
    assert!(c.identique, "Pure : la source à l'échantillon près ({c:?})");
}

/// La garde de volume #8920 est INCHANGÉE par la chaîne : ampli monté
/// au-dessus de la sécurité, une lecture avec crossfeed envoie à l'ampli
/// exactement les mêmes consignes qu'une lecture sans chaîne, et aucune
/// au-dessus de la sécurité.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_garde_de_volume_air_est_inchangee_par_la_chaine_t12c() {
    for ampli_db in [-20.0, -50.0] {
        let sans = jouer_sur_air(Chaine::Aucune, ampli_db).await;
        let avec = jouer_sur_air(Chaine::Crossfeed, ampli_db).await;
        println!(
            "T12c garde AIR, ampli {ampli_db} dB : sans chaîne {:?}, avec {:?}",
            sans.volumes, avec.volumes
        );
        assert_eq!(
            sans.volumes, avec.volumes,
            "ampli à {ampli_db} dB : la chaîne ne doit rien changer aux consignes de volume"
        );
        for v in &avec.volumes {
            assert!(
                f64::from(*v) <= SECURITE_DB + 1e-6,
                "l'ampli a reçu {v} dB, au-dessus de la sécurité ({SECURITE_DB} dB)"
            );
        }
    }
}
