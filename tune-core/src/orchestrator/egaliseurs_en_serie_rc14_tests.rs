//! rc14 (retours de Rémi) — de bout en bout dans le moteur :
//! - graphique ET paramétrique cochés dans la chaîne TESS sont tous deux
//!   appliqués, en série, dans l'ordre de la chaîne ;
//! - « Bypass all » rend un flux identique, à l'octet, à celui d'une zone sans
//!   aucun traitement (égaliseurs, crossfeed, headroom manuel compris) ;
//! - une zone d'avant (seulement `zone_{id}_eq_profile`) joue comme avant.

use super::PlaybackOrchestrator;
use crate::audio::eq::{EqBandSpec, EqProfile};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::tess::chaine::{ChaineTess, Headroom, MaillonGreffon};
use crate::tess::migration::{courbe_db, ecrire_chaine, migrer_zone};
use serde_json::{Value, json};
use std::sync::Arc;

const SR: u32 = 48_000;

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

fn settings(o: &PlaybackOrchestrator) -> SettingsRepo {
    SettingsRepo::with_backend(o.db.clone())
}

/// Une zone réseau (DLNA), égaliseur et crossfeed installés.
fn zone(o: &PlaybackOrchestrator, nom: &str) -> i64 {
    let s = settings(o);
    for id in ["equalizer", "crossfeed"] {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    }
    ZoneRepo::with_backend(o.db.clone())
        .create(nom, Some("dlna"), Some(&format!("uuid:{nom}-rc14")))
        .unwrap()
}

fn bande(freq: f64, gain: f64, q: f64, t: &str) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        channel: None,
    }
}

fn graphique() -> EqProfile {
    let gains = [6.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0];
    EqProfile {
        enabled: true,
        bands: crate::audio::eq_presets::GRILLE_10
            .iter()
            .zip(gains)
            .map(|(&f, g)| bande(f, g, 1.0, "peak"))
            .collect(),
        ..Default::default()
    }
}

fn parametrique() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![
            bande(3150.0, -28.0, 6.0, "peak"),
            bande(120.0, -4.0, 0.7, "low_shelf"),
        ],
        ..Default::default()
    }
}

fn maillon(id: &str, p: &EqProfile) -> MaillonGreffon {
    let mut v = serde_json::to_value(p).unwrap();
    v.as_object_mut().unwrap().remove("enabled");
    MaillonGreffon {
        id: id.into(),
        enabled: true,
        settings: v,
    }
}

fn crossfeed() -> MaillonGreffon {
    MaillonGreffon {
        id: "crossfeed".into(),
        enabled: true,
        settings: json!({"amount": 0.4, "delay_ms": 0.3}),
    }
}

/// 24 bits stéréo : un sinus par voie (voies différentes, pour que le
/// crossfeed agisse), une seconde.
fn pcm() -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..SR as usize {
        let t = i as f64 / f64::from(SR);
        for (f, a) in [(1000.0, 0.5), (3150.0, 0.3)] {
            let x = ((2.0 * std::f64::consts::PI * f * t).sin() * a * 8_388_607.0) as i32;
            v.extend_from_slice(&x.to_le_bytes()[..3]);
        }
    }
    v
}

/// Le flux que la zone sert (relais réseau : égaliseur, convolution,
/// crossfeed, compensation, ReplayGain).
fn servi(o: &PlaybackOrchestrator, zone_id: i64) -> Vec<u8> {
    let mut chaine = o.load_streaming_dsp(zone_id, crate::audio::replaygain::PisteRg::AUCUNE, SR, 2);
    let mut x = pcm();
    for bloc in x.chunks_mut(6 * 4096) {
        chaine.process(bloc, 24);
    }
    x
}

#[tokio::test]
async fn graphique_et_parametrique_sonnent_ensemble_en_serie_rc14() {
    let o = orchestrateur();
    let z = zone(&o, "remi");
    let (g, p) = (graphique(), parametrique());
    let chaine = ChaineTess {
        plugins: vec![
            maillon("equalizer-graphic", &g),
            crossfeed(),
            maillon("equalizer-parametric", &p),
        ],
        ..ChaineTess::default()
    };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    // L'ancienne clé porte le principal (le graphique) : les anciens clients.
    let ancien: EqProfile = serde_json::from_str(
        &settings(&o)
            .get(&format!("zone_{z}_eq_profile"))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ancien.bands.len(), g.bands.len());

    let moteur = o.load_eq_profile(z).expect("égaliseur du moteur");
    let freqs: Vec<f64> = g.bands.iter().chain(&p.bands).map(|b| b.freq).collect();
    assert_eq!(
        moteur.bands.iter().map(|b| b.freq).collect::<Vec<_>>(),
        freqs,
        "les deux égaliseurs, dans l'ordre de la chaîne"
    );
    // Réponse combinée = somme des réponses, à chaque fréquence.
    let f = [31.0, 63.0, 120.0, 250.0, 1000.0, 3150.0, 8000.0, 16000.0];
    let (rg, rp, rm) = (courbe_db(&g, &f), courbe_db(&p, &f), courbe_db(&moteur, &f));
    for i in 0..f.len() {
        assert!(
            (rm[0][i] - rg[0][i] - rp[0][i]).abs() < 1e-6,
            "{} Hz : {} ≠ {} + {}",
            f[i],
            rm[0][i],
            rg[0][i],
            rp[0][i]
        );
    }
    // Le creux à −28 dB du paramétrique est entendu (le moteur bornait à −24).
    assert!(rp[0][5] < -27.0, "{}", rp[0][5]);

    // Ordre inversé dans la chaîne : bandes inversées.
    let inverse = ChaineTess {
        plugins: vec![
            maillon("equalizer-parametric", &p),
            crossfeed(),
            maillon("equalizer-graphic", &g),
        ],
        ..ChaineTess::default()
    };
    ecrire_chaine(&settings(&o), z, &inverse).unwrap();
    let moteur = o.load_eq_profile(z).unwrap();
    let freqs: Vec<f64> = p.bands.iter().chain(&g.bands).map(|b| b.freq).collect();
    assert_eq!(moteur.bands.iter().map(|b| b.freq).collect::<Vec<_>>(), freqs);

    // Un seul coché : lui seul.
    let mut un = inverse.clone();
    un.plugins[0].enabled = false;
    ecrire_chaine(&settings(&o), z, &un).unwrap();
    assert_eq!(o.load_eq_profile(z).unwrap().bands.len(), g.bands.len());
}

#[tokio::test]
async fn le_headroom_manuel_est_la_seule_marge_rc14() {
    let o = orchestrateur();
    let z = zone(&o, "marge");
    let mut chaine = ChaineTess {
        plugins: vec![maillon("equalizer-graphic", &graphique())],
        ..ChaineTess::default()
    };
    chaine.fixed.headroom = Headroom::Manual { db: -6.0 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    let p = o.load_eq_profile(z).unwrap();
    assert_eq!(p.reserve_imposee_db, Some(-6.0));
    let proc = crate::audio::eq::EqProcessor::new(&p, SR, 2);
    assert_eq!(proc.preamp_db(0), Some(-6.0));
    // Auto : la réserve calculée de l'égaliseur, comme avant.
    chaine.fixed.headroom = Headroom::Auto {};
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    let p = o.load_eq_profile(z).unwrap();
    assert_eq!(p.reserve_imposee_db, None);
    let attendu = crate::audio::eq::EqProcessor::new(&graphique(), SR, 2).preamp_db(0);
    assert_eq!(
        crate::audio::eq::EqProcessor::new(&p, SR, 2).preamp_db(0),
        attendu
    );
}

/// « Bypass all » : le flux servi est, à l'octet, celui d'une zone sans aucun
/// traitement — et l'entrée elle-même. Contre-épreuve : sans Bypass all, la
/// même chaîne change le flux.
#[tokio::test]
async fn bypass_all_rend_le_flux_sans_traitement_a_l_echantillon_pres_rc14() {
    let o = orchestrateur();
    let nue = zone(&o, "nue");
    let z = zone(&o, "bypass");
    let mut chaine = ChaineTess {
        plugins: vec![
            maillon("equalizer-graphic", &graphique()),
            maillon("equalizer-parametric", &parametrique()),
            crossfeed(),
        ],
        ..ChaineTess::default()
    };
    chaine.fixed.headroom = Headroom::Manual { db: -6.0 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    // Repli mono et cadence fixe posés aussi (anciennes clés).
    settings(&o)
        .set(&format!("zone_{z}_mono_downmix"), "true")
        .unwrap();
    settings(&o)
        .set(
            &crate::audio::frequence_fixe::cle_de_zone(z),
            "96000",
        )
        .unwrap();
    let source = pcm();
    let traite = servi(&o, z);
    assert_ne!(traite, source, "contre-épreuve : la chaîne traite");

    chaine.bypass_all = true;
    let bilan = ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    assert!(
        bilan
            .cles_modifiees
            .contains(&crate::tess::cle_chaine_zone(z)),
        "la route doit rafraîchir la sortie vivante : {bilan:?}"
    );
    assert!(o.load_eq_profile(z).is_none());
    assert!(o.load_crossfeed_processor(z, SR).is_none());
    assert!(!PlaybackOrchestrator::zone_mono_downmix_with(&o.db, z));
    assert_eq!(crate::audio::frequence_fixe::demandee_active(&o.db, z), None);
    let contourne = servi(&o, z);
    assert_eq!(contourne, source, "Bypass all a laissé un traitement");
    assert_eq!(contourne, servi(&o, nue));

    // Décoché : tout revient.
    chaine.bypass_all = false;
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    assert_eq!(servi(&o, z), traite);
}

/// Zone d'avant TESS : seulement `zone_{id}_eq_profile`. Le moteur le lit tel
/// quel, avant et après la migration de la zone (une seule façade).
#[tokio::test]
async fn une_zone_d_avant_joue_comme_avant_rc14() {
    let o = orchestrateur();
    let z = zone(&o, "avant");
    let brut = serde_json::to_string(&parametrique()).unwrap();
    settings(&o)
        .set(&format!("zone_{z}_eq_profile"), &brut)
        .unwrap();
    let avant = serde_json::to_value(o.load_eq_profile(z).unwrap()).unwrap();
    assert_eq!(avant, serde_json::from_str::<Value>(&brut).unwrap());
    migrer_zone(&settings(&o), z).unwrap();
    assert!(crate::tess::egaliseurs_en_serie::egaliseurs_de_zone(&settings(&o), z).is_none());
    assert_eq!(serde_json::to_value(o.load_eq_profile(z).unwrap()).unwrap(), avant);
    assert_eq!(
        settings(&o).get(&format!("zone_{z}_eq_profile")).unwrap(),
        Some(brut)
    );
}
