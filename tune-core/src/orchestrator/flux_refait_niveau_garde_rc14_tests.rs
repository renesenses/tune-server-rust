//! rc14 (sécurité auditive, décision de Bertrand du 25/11) — un flux RÉSEAU
//! refait (relance après un réglage : égaliseur coupé, PURE) ne repart plus
//! au signal sec après le blanc de la relance. Volume monté pendant que
//! l'égaliseur jouait : le flux refait démarre au niveau du flux quitté
//! (préampli gardé reporté), sans jamais le dépasser, voyant compris ; il ne
//! rejoint le sec que si le volume redescend au niveau d'avant l'égaliseur.
//! Volume inchangé : rien n'est reporté, le flux refait est le sec, à l'octet.

use super::PlaybackOrchestrator;
use super::eq_en_vol::EqEnVol;
use crate::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOC: usize = 2_048;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

fn profil() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![EqBandSpec {
            freq: 80.0,
            gain: 12.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Une zone DLNA au volume `volume` (0..100), égaliseur installé et réglé.
fn zone(o: &PlaybackOrchestrator, volume: f64) -> i64 {
    let s = SettingsRepo::with_backend(o.db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    let z = ZoneRepo::with_backend(o.db.clone())
        .create("salon", Some("dlna"), Some("uuid:salon-rc14-refait"))
        .unwrap();
    ZoneRepo::with_backend(o.db.clone())
        .update_volume(z, volume)
        .unwrap();
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&profil()).unwrap(),
    )
    .unwrap();
    z
}

fn couper_l_egaliseur(o: &PlaybackOrchestrator, z: i64) {
    let mut p = profil();
    p.enabled = false;
    SettingsRepo::with_backend(o.db.clone())
        .set(
            &format!("zone_{z}_eq_profile"),
            &serde_json::to_string(&p).unwrap(),
        )
        .unwrap();
    assert!(o.load_eq_processor(z, SR, 2).is_none(), "prémisse : coupé");
}

/// Le flux qui jouait : un relais dont le porteur a joué l'égaliseur. Rend
/// le préampli de cet égaliseur (dB).
fn flux_avec_egaliseur(o: &PlaybackOrchestrator, z: i64, sid: &str) -> (Arc<EqEnVol>, f64) {
    let poste = EqEnVol::pour_le_relais(false);
    o.noter_eq_en_vol(z, sid, poste.clone());
    let mut eq = o.load_eq_processor(z, SR, 2);
    let preampli = eq.as_ref().and_then(|e| e.preamp_db(0)).expect("égaliseur");
    assert!(preampli < -6.0, "prémisse : réserve de {preampli:.2} dB");
    for _ in 0..4 {
        poste.relever(&mut eq, SR, 2);
    }
    (poste, preampli)
}

fn en_lecture(sid: &str) -> crate::playback::NowPlaying {
    crate::playback::NowPlaying {
        stream_id: Some(sid.into()),
        ..Default::default()
    }
}

/// Sinus 1 kHz stéréo 16 bits, bloc `n`.
fn bloc(n: usize) -> Vec<u8> {
    (n * BLOC..(n + 1) * BLOC)
        .flat_map(|i| {
            let v = (0.25
                * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(SR)).sin()
                * 32_767.0) as i16;
            [v.to_le_bytes(), v.to_le_bytes()].concat()
        })
        .collect()
}

fn rms(pcm: &[u8]) -> f64 {
    let v: Vec<f64> = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .map(|t| f64::from(i16::from_le_bytes([t[0], t[1]])))
        .collect();
    (v.iter().map(|s| s * s).sum::<f64>() / v.len() as f64).sqrt()
}

/// Volume monté pendant l'égaliseur, puis égaliseur coupé : le flux refait
/// démarre au niveau du préampli gardé, et y reste (jamais au-dessus) ; le
/// voyant le dit. Avant : le flux refait était le sec, +préampli d'un coup.
#[tokio::test]
async fn flux_refait_volume_monte_reprend_le_niveau_garde_rc14() {
    let o = orchestrateur();
    let z = zone(&o, 25.0);
    let (_ancien, preampli) = flux_avec_egaliseur(&o, z, "flux-a");
    // L'auditeur monte le volume pour compenser la réserve.
    ZoneRepo::with_backend(o.db.clone())
        .update_volume(z, 50.0)
        .unwrap();
    o.noter_le_volume_aux_postes(z, 0.5);
    couper_l_egaliseur(&o, z);
    o.retenir_le_niveau_du_flux_refait(z, Some(&en_lecture("flux-a")));
    assert!(
        o.niveau_reseau_a_reporter(z).is_some(),
        "volume monté : le niveau gardé doit être reporté"
    );
    // Le routage : le flux refait passe par le flux traité.
    let mut dsp = o.load_streaming_dsp(z, crate::audio::replaygain::PisteRg::AUCUNE, SR, 2);
    assert!(dsp.is_active(), "le flux refait porte le niveau gardé");
    let nouveau = EqEnVol::pour_le_relais(false);
    o.noter_eq_en_vol(z, "flux-b", nouveau.clone());
    dsp.en_vol = Some(nouveau.clone());
    let sec = rms(&bloc(0));
    for n in 0..60 {
        let mut b = bloc(n);
        dsp.process(&mut b, 16);
        let db = 20.0 * (rms(&b) / sec).log10();
        assert!(
            db <= preampli + 0.1,
            "flux refait, bloc {n} : {db:+.2} dB, au-dessus du niveau gardé {preampli:+.2} dB"
        );
        assert!(
            (db - preampli).abs() < 0.3,
            "flux refait, bloc {n} : {db:+.2} dB, attendu le niveau gardé {preampli:+.2} dB"
        );
    }
    let garde = nouveau.preampli_garde().expect("voyant « niveau réduit »");
    assert!((garde.db - preampli).abs() < 0.3, "{garde:?}");
    // Le volume redescend au niveau d'avant l'égaliseur : retour au sec,
    // par la rampe, sans dépasser ce niveau, puis à l'octet.
    o.noter_le_volume_aux_postes(z, 0.25);
    for n in 60..400 {
        let mut b = bloc(n);
        dsp.process(&mut b, 16);
        assert!(rms(&b) <= sec * 1.012, "bloc {n} au-dessus du sec");
    }
    let mut b = bloc(400);
    dsp.process(&mut b, 16);
    assert_eq!(b, bloc(400), "revenu au niveau d'avant : le sec, à l'octet");
    assert!(nouveau.preampli_garde().is_none());
}

/// Contre-épreuve : volume inchangé, rien n'est reporté ; le flux refait est
/// le sec, à l'octet (le sec ne dépasse pas le niveau d'avant l'égaliseur).
#[tokio::test]
async fn flux_refait_volume_inchange_rien_n_est_reporte_rc14() {
    let o = orchestrateur();
    let z = zone(&o, 25.0);
    flux_avec_egaliseur(&o, z, "flux-a");
    couper_l_egaliseur(&o, z);
    o.retenir_le_niveau_du_flux_refait(z, Some(&en_lecture("flux-a")));
    assert!(o.niveau_reseau_a_reporter(z).is_none());
    let mut dsp = o.load_streaming_dsp(z, crate::audio::replaygain::PisteRg::AUCUNE, SR, 2);
    assert!(!dsp.is_active());
    let mut b = bloc(3);
    dsp.process(&mut b, 16);
    assert_eq!(b, bloc(3));
}

/// Le flux quitté n'avait pas de poste (fichier pré-transcodé) : son niveau
/// vient de l'égaliseur de son empreinte, sa référence du volume noté à sa
/// création.
#[tokio::test]
async fn flux_refait_sans_poste_lit_l_empreinte_rc14() {
    let o = orchestrateur();
    let z = zone(&o, 25.0);
    let preampli = o
        .load_eq_processor(z, SR, 2)
        .and_then(|e| e.preamp_db(0))
        .unwrap();
    let empreinte = o.empreinte_du_traitement(z, crate::audio::replaygain::PisteRg::AUCUNE);
    let lu = super::dsp::niveau_de_l_empreinte(&empreinte);
    assert!(
        (20.0 * lu.log10() - preampli).abs() < 0.05,
        "empreinte : {:.2} dB pour {preampli:.2} dB",
        20.0 * lu.log10()
    );
    o.noter_traitement_du_flux(z, "fichier-a", empreinte);
    ZoneRepo::with_backend(o.db.clone())
        .update_volume(z, 40.0)
        .unwrap();
    couper_l_egaliseur(&o, z);
    o.retenir_le_niveau_du_flux_refait(z, Some(&en_lecture("fichier-a")));
    let report = o.niveau_reseau_a_reporter(z).expect("report");
    assert_eq!(report.volume_avant, Some(250));
    assert!((20.0 * report.niveau.log10() - preampli).abs() < 0.05);
    assert_eq!(super::dsp::niveau_de_l_empreinte("pure"), 1.0);
    let _ = EqProcessor::neutre_qui_garde(SR, 2, report.niveau, 0.0).expect("neutre");
}
