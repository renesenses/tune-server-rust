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
            bande(3150.0, -28.0, 2.0, "peak"),
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
    let mut chaine =
        o.load_streaming_dsp(zone_id, crate::audio::replaygain::PisteRg::AUCUNE, SR, 2);
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
    assert!(rp[0][5] < -27.5, "{}", rp[0][5]);

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
    assert_eq!(
        moteur.bands.iter().map(|b| b.freq).collect::<Vec<_>>(),
        freqs
    );

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
    let sure = crate::audio::eq::EqProcessor::new(&graphique(), SR, 2)
        .preamp_db(0)
        .unwrap();
    assert_eq!(proc.preamp_db(0), Some((-6.0f64).min(sure)));
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
        .set(&crate::audio::frequence_fixe::cle_de_zone(z), "96000")
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
    assert_eq!(
        crate::audio::frequence_fixe::demandee_active(&o.db, z),
        None
    );
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
    assert!(crate::tess::egaliseurs_en_serie::egaliseurs_de_zone(&settings(&o), z, true).is_none());
    assert_eq!(
        serde_json::to_value(o.load_eq_profile(z).unwrap()).unwrap(),
        avant
    );
    assert_eq!(
        settings(&o).get(&format!("zone_{z}_eq_profile")).unwrap(),
        Some(brut)
    );
}

/// Revue #9520 (P1) — « Bypass all » ne lève JAMAIS la protection du niveau
/// DoP (#8813) : volume à 6 %, la conversion logicielle reste demandée.
#[tokio::test]
async fn revue_9520_bypass_all_garde_la_protection_dop_8813() {
    use crate::orchestrator::TransportDsd;
    let o = orchestrateur();
    let zones = ZoneRepo::with_backend(o.db.clone());
    let z = zones
        .create("DoP 6 %", Some("local"), Some("local:dop-9520"))
        .unwrap();
    zones.update_volume(z, 6.0).unwrap();
    o.playback.set_volume(z, 0.06).await;
    let mut chaine = ChaineTess {
        plugins: vec![crossfeed()],
        ..ChaineTess::default()
    };
    chaine.bypass_all = true;
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    assert!(crate::audio::audiophile::traitements_contournes(&o.db, z));
    for transport in [TransportDsd::Dop, TransportDsd::NatifServiEnDop] {
        assert!(
            o.attenuation_logicielle_demandee_pour_dop(
                z,
                transport,
                crate::audio::replaygain::PisteRg::AUCUNE
            )
            .await,
            "Bypass all a levé la protection DoP : {transport:?}"
        );
    }
    // Les rampes anti-clic restent aussi sous Bypass all (outil de
    // comparaison, pas du bit-perfect) ; PURE, lui, les retire.
    settings(&o)
        .set(&format!("zone_{z}_soft_mute_ms"), "40")
        .unwrap();
    assert!(PlaybackOrchestrator::zone_soft_mute_ms_with(&o.db, z) > 0);
    settings(&o)
        .set(&format!("zone_{z}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert_eq!(PlaybackOrchestrator::zone_soft_mute_ms_with(&o.db, z), 0);
}

/// Revue #9520 (3) — headroom manuel plus FAIBLE que la réserve « Sûre » :
/// la réserve calculée est gardée (la plus forte des deux).
#[tokio::test]
async fn le_headroom_manuel_ne_retire_jamais_la_reserve_sure_9520() {
    let o = orchestrateur();
    let z = zone(&o, "reserve-sure");
    let mut chaine = ChaineTess {
        plugins: vec![maillon("equalizer-graphic", &graphique())],
        ..ChaineTess::default()
    };
    chaine.fixed.headroom = Headroom::Manual { db: -0.5 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    let p = o.load_eq_profile(z).unwrap();
    let sure = crate::audio::eq::EqProcessor::new(&graphique(), SR, 2)
        .preamp_db(0)
        .unwrap();
    assert!(sure < -0.5, "{sure}");
    assert_eq!(
        crate::audio::eq::EqProcessor::new(&p, SR, 2).preamp_db(0),
        Some(sure)
    );
}

/// Revue #9520 (4) — headroom manuel sans égaliseur coché : un gain fixe,
/// réellement appliqué (même sans le greffon égaliseur installé).
#[tokio::test]
async fn le_headroom_manuel_sans_egaliseur_est_un_gain_fixe_9520() {
    let o = orchestrateur();
    // Sortie LOCALE : c'est là, et seulement là, que vit le gain fixe.
    let z = ZoneRepo::with_backend(o.db.clone())
        .create("gain-fixe", Some("local"), Some("local:gain-fixe-9520"))
        .unwrap();
    let mut chaine = ChaineTess {
        plugins: vec![crossfeed()],
        ..ChaineTess::default()
    };
    chaine.fixed.headroom = Headroom::Manual { db: -6.0 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    let p = o.load_eq_profile(z).expect("gain fixe du headroom manuel");
    assert!(p.bands.is_empty());
    let mut proc = crate::audio::eq::EqProcessor::new(&p, SR, 2);
    assert!(proc.is_enabled());
    let mut x = vec![0.5f64, -0.25, 0.125, 0.0];
    proc.process_interleaved_f64(&mut x);
    let g = 10f64.powf(-6.0 / 20.0);
    for (a, b) in x.iter().zip([0.5, -0.25, 0.125, 0.0]) {
        assert!((a - b * g).abs() < 1e-12, "{a} vs {}", b * g);
    }
}

/// Revue #9520 (5) — une série purement soustractive a sa réserve L1 : pas
/// de falaise quand une bande passe de −0,01 à +0,01 dB.
#[test]
fn une_serie_soustractive_n_a_pas_de_falaise_9520() {
    let creux = |g: f64| EqProfile {
        enabled: true,
        bands: vec![
            bande(1000.0, -20.0, 8.0, "peak"),
            bande(4000.0, g, 1.0, "peak"),
        ],
        ..Default::default()
    };
    let serie = |g: f64| {
        crate::tess::egaliseurs_en_serie::combiner_en_serie(&[creux(g), parametrique()]).unwrap()
    };
    let pre = |p: &EqProfile| {
        crate::audio::eq::EqProcessor::new(p, SR, 2)
            .preamp_db(0)
            .unwrap()
    };
    let (a, b) = (pre(&serie(-0.01)), pre(&serie(0.01)));
    assert!((a - b).abs() < 0.05, "falaise : {a} → {b}");
}

/// Revue #9520 — la voie LOCALE réelle : `refresh_zone_pure_dsp` pose
/// `pure_bypass` et vide l'égaliseur sous « Bypass all », et les remet en le
/// décochant.
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn bypass_all_sur_la_voie_locale_reelle_9520() {
    let o = orchestrateur();
    let s = settings(&o);
    for id in ["equalizer", "crossfeed"] {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    }
    let z = ZoneRepo::with_backend(o.db.clone())
        .create("Local 9520", Some("local"), Some("local:Local 9520"))
        .unwrap();
    o.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "Local 9520".to_string(),
        )));
    {
        let arc = o.outputs.lock().await.get("local:Local 9520").unwrap();
        let sortie = arc.lock().await;
        let local = sortie
            .as_any()
            .downcast_ref::<crate::outputs::local::LocalOutput>()
            .unwrap();
        local.declare_current_format_for_test(44_100, 2);
        local.set_pure_bypass(false);
    }
    let mut chaine = ChaineTess {
        plugins: vec![
            maillon("equalizer-graphic", &graphique()),
            maillon("equalizer-parametric", &parametrique()),
            crossfeed(),
        ],
        ..ChaineTess::default()
    };
    let etat = |o: &PlaybackOrchestrator| {
        let o = o.outputs.clone();
        async move {
            let arc = o.lock().await.get("local:Local 9520").unwrap();
            let sortie = arc.lock().await;
            let local = sortie
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
                .unwrap();
            (local.pure_bypass_for_test(), local.has_eq())
        }
    };
    ecrire_chaine(&s, z, &chaine).unwrap();
    assert!(o.refresh_zone_pure_dsp(z).await);
    assert_eq!(etat(&o).await, (false, true));
    chaine.bypass_all = true;
    ecrire_chaine(&s, z, &chaine).unwrap();
    assert!(o.refresh_zone_pure_dsp(z).await);
    let (contourne, _) = etat(&o).await;
    assert!(contourne, "Bypass all doit poser pure_bypass sur la sortie");
    chaine.bypass_all = false;
    ecrire_chaine(&s, z, &chaine).unwrap();
    assert!(o.refresh_zone_pure_dsp(z).await);
    assert_eq!(etat(&o).await, (false, true));
}

/// Revue #9520 v2 (MAJEUR) — headroom manuel seul sur une zone DLNA : ni
/// égaliseur actif, ni flux « traité hors TESS », et des octets identiques au
/// flux sans traitement (pas de transcodage, pas de gain).
#[tokio::test]
async fn le_headroom_manuel_seul_garde_le_reseau_bit_perfect_9520() {
    let o = orchestrateur();
    let z = zone(&o, "dlna-headroom-seul");
    let avant = servi(&o, z);
    assert_eq!(avant, pcm());
    let mut chaine = ChaineTess::default();
    chaine.fixed.headroom = Headroom::Manual { db: -6.0 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    assert!(!o.zone_has_active_eq(z), "égaliseur annoncé actif sur le réseau");
    assert!(!o.zone_traite_le_flux_hors_tess(z), "flux traité hors TESS");
    assert!(o.load_eq_profile(z).is_none());
    assert_eq!(servi(&o, z), avant, "octets changés par le headroom seul");
}

/// Revue #9520 v2 — l'empreinte du traitement voit la marge manuelle : un
/// headroom changé relance un flux réseau en cours (#4407).
#[tokio::test]
async fn l_empreinte_voit_le_headroom_manuel_9520() {
    let o = orchestrateur();
    let z = zone(&o, "empreinte-headroom");
    let mut chaine = ChaineTess {
        plugins: vec![
            maillon("equalizer-graphic", &graphique()),
            maillon("equalizer-parametric", &parametrique()),
        ],
        ..ChaineTess::default()
    };
    let empreinte =
        |o: &PlaybackOrchestrator| o.empreinte_du_traitement(z, crate::audio::replaygain::PisteRg::AUCUNE);
    chaine.fixed.headroom = Headroom::Manual { db: -3.0 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    let a = empreinte(&o);
    chaine.fixed.headroom = Headroom::Manual { db: -24.0 };
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    let b = empreinte(&o);
    assert_ne!(a, b, "un headroom changé doit changer l'empreinte");
    chaine.fixed.headroom = Headroom::Auto {};
    ecrire_chaine(&settings(&o), z, &chaine).unwrap();
    assert_ne!(empreinte(&o), b);
}
