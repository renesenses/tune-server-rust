//! Témoins du lot T16 : le chemin du signal ne ment jamais.

use super::*;
use crate::tess::chaine::{EtagesFixes, Headroom, MaillonGreffon, Quantification, TauxDsd};
use crate::tess::etage::{EtageTess, FormatFlux};
use crate::tess::etages_fixes::{ConsigneVolume, EtageHeadroom, EtageVolume, lineaire};
use crate::tess::greffon::catalogue_de_reference;
use crate::tess::resolution::{ContexteExecution, ControleVolume, SourceFlux, VolumeZone};
use crate::tess::zones::TransportZone;
use serde_json::json;

fn tout_est_installe(_: &str) -> bool {
    true
}

fn ctx(transport: TransportZone, pure: bool, volume_db: f64) -> ContexteExecution<'static> {
    ContexteExecution {
        transport,
        source: Some(SourceFlux::Pcm {
            sample_rate_hz: 96_000,
            bits: 24,
            channels: 2,
        }),
        premium: true,
        greffon_disponible: &tout_est_installe,
        pure,
        volume: VolumeZone {
            control: ControleVolume::Software,
            db: volume_db,
        },
    }
}

fn maillon(id: &str, enabled: bool, settings: Value) -> MaillonGreffon {
    MaillonGreffon {
        id: id.into(),
        enabled,
        settings,
    }
}

/// La chaîne de la maquette A4 : convolution, puis égaliseur 31 bandes.
fn maquette_a4() -> ChaineTess {
    ChaineTess {
        plugins: vec![
            maillon(
                "convolution",
                true,
                json!({ "ir_path": "/var/lib/tune/ir/room_96k.wav" }),
            ),
            maillon(
                "equalizer-graphic",
                true,
                json!({ "bands": vec![json!({}); 31], "preset": "Jazz" }),
            ),
            maillon("crossfeed", false, Value::Null),
        ],
        ..ChaineTess::default()
    }
}

fn service(greffons: &[&str], headroom_db: Option<f64>, volume: bool) -> EnService {
    EnService {
        greffons: GreffonsEnService::Exacts(greffons.iter().map(|s| s.to_string()).collect()),
        headroom_db,
        upsampling: false,
        volume_logiciel: volume,
        latences: vec![],
    }
}

fn chemin(
    c: &ChaineTess,
    x: &ContexteExecution<'_>,
    s: &EnService,
    intact: Option<bool>,
) -> CheminTess {
    let cat = catalogue_de_reference();
    let r = c.resoudre(&cat, x);
    chemin_du_signal(c, &cat, r, s, intact, None)
}

fn noms_effectifs(c: &CheminTess) -> Vec<&str> {
    c.effectives().map(|e| e.name.as_str()).collect()
}

fn etape<'a>(c: &'a CheminTess, code: CodeEtape) -> &'a EtapeNommee {
    c.steps
        .iter()
        .find(|e| e.etape.code == code)
        .expect("étape")
}

fn greffon<'a>(c: &'a CheminTess, id: &str) -> &'a EtapeNommee {
    c.steps
        .iter()
        .find(|e| e.etape.plugin_id.as_deref() == Some(id))
        .expect("greffon")
}

#[test]
fn maquette_a4_etapes_nommees_parametres_et_ordre_reel_de_l_executeur() {
    // L'exécuteur appelle l'égaliseur AVANT la convolution : c'est cet ordre
    // qui est publié, pas celui de la liste.
    let s = service(&["equalizer-graphic", "convolution"], Some(-3.0), true);
    let c = chemin(
        &maquette_a4(),
        &ctx(TransportZone::Oaat, false, -18.0),
        &s,
        Some(false),
    );
    assert_eq!(
        noms_effectifs(&c),
        [
            "Source",
            "Headroom",
            "Graphic EQ",
            "Convolution",
            "Volume control",
            "Dither",
            "Output"
        ]
    );
    assert_eq!(c.badge, Badge::ProcessedByTess);
    assert_eq!(c.headroom_db, -3.0);
    let conv = greffon(&c, "convolution").etape.params.clone().unwrap();
    assert_eq!(conv["filter"], "room_96k.wav");
    let eq = greffon(&c, "equalizer-graphic")
        .etape
        .params
        .clone()
        .unwrap();
    assert_eq!(eq["bands"], 31);
    assert_eq!(eq["preset"], "Jazz");
    // Q9 : Upsampling avant Volume, Dither en dernier avant la sortie.
    let codes: Vec<_> = c.steps.iter().map(|e| e.etape.code).collect();
    let pos = |k| codes.iter().position(|x| *x == k).unwrap();
    assert!(pos(CodeEtape::Upsampling) < pos(CodeEtape::Volume));
    assert!(pos(CodeEtape::Volume) < pos(CodeEtape::Quantization));
    assert_eq!(c.volume.placement, Some(PlacementVolume::AtRender));
    assert!(c.discrepancies.is_empty(), "{:?}", c.discrepancies);
}

#[test]
fn demande_mais_pas_en_service_est_eteint_et_ne_compte_plus() {
    // La chaîne demande convolution et égaliseur, rien ne tourne, volume à
    // 0 dB : bit-perfect, et chaque écart est publié.
    let s = service(&[], None, false);
    let c = chemin(
        &maquette_a4(),
        &ctx(TransportZone::Local, false, 0.0),
        &s,
        Some(true),
    );
    for id in ["convolution", "equalizer-graphic"] {
        let g = &greffon(&c, id).etape;
        assert!(!g.effective);
        assert_eq!(g.reason, Some(MotifEtape::NotInService));
    }
    assert_eq!(
        etape(&c, CodeEtape::Headroom).etape.reason,
        Some(MotifEtape::NotInService)
    );
    assert_eq!(c.badge, Badge::BitPerfect);
    assert_eq!(c.headroom_db, 0.0);
    assert_eq!(c.discrepancies.len(), 3);
    assert!(
        c.discrepancies
            .iter()
            .all(|e| e.kind == SensEcart::RequestedNotRunning)
    );
}

#[test]
fn en_service_mais_pas_demande_est_publie_et_retire_le_bit_perfect() {
    // Chaîne vide, volume à 0 dB : la résolution dit bit-perfect. Mais un
    // étage tourne (et un volume) : le chemin le dit.
    let s = service(&["crossfeed"], None, true);
    let c = chemin(
        &ChaineTess::default(),
        &ctx(TransportZone::Local, false, 0.0),
        &s,
        Some(true),
    );
    assert!(greffon(&c, "crossfeed").etape.effective);
    assert!(etape(&c, CodeEtape::Volume).etape.effective);
    assert!(etape(&c, CodeEtape::Quantization).etape.effective);
    assert_eq!(c.badge, Badge::ProcessedByTess);
    assert_eq!(c.discrepancies.len(), 2);
}

#[test]
fn pure_le_volume_logiciel_reste_effectif_bit_perfect_seulement_a_0_db() {
    let chaine = maquette_a4();
    // Pure à −18 dB : greffons éteints, volume effectif (décision T07).
    let c = chemin(
        &chaine,
        &ctx(TransportZone::Local, true, -18.0),
        &service(&[], None, true),
        Some(true),
    );
    assert_eq!(
        greffon(&c, "convolution").etape.reason,
        Some(MotifEtape::PureMode)
    );
    assert!(etape(&c, CodeEtape::Volume).etape.effective);
    assert_eq!(c.badge, Badge::ProcessedByTess);
    // Pure à 0 dB (ou tenu à 100 % par le verrou) : bit-perfect.
    let c = chemin(
        &chaine,
        &ctx(TransportZone::Local, true, 0.0),
        &service(&[], None, false),
        Some(true),
    );
    assert_eq!(c.badge, Badge::BitPerfect);
    assert!(c.discrepancies.is_empty());
    assert_eq!(
        etape(&c, CodeEtape::Quantization).etape.reason,
        Some(MotifEtape::NothingToQuantize)
    );
}

#[test]
fn un_chemin_altere_hors_tess_n_est_jamais_dit_bit_perfect() {
    let c = chemin(
        &ChaineTess::default(),
        &ctx(TransportZone::Local, false, 0.0),
        &service(&[], None, false),
        Some(false),
    );
    assert_eq!(c.badge, Badge::AlteredOutsideTess);
}

#[test]
fn hors_tess_rien_d_effectif_sauf_ce_qui_tourne_vraiment() {
    let c = chemin(
        &maquette_a4(),
        &ctx(TransportZone::HqPlayer, false, -10.0),
        &service(&[], None, false),
        None,
    );
    assert_eq!(c.badge, Badge::OutsideTess);
    assert_eq!(noms_effectifs(&c), ["Source", "Output"]);
    // Un traitement qui tourne malgré tout ne se tait pas.
    let c = chemin(
        &maquette_a4(),
        &ctx(TransportZone::HqPlayer, false, -10.0),
        &service(&["equalizer-graphic"], None, false),
        None,
    );
    assert_eq!(c.badge, Badge::AlteredOutsideTess);
}

#[test]
fn sonde_globale_ordre_historique_hors_service_et_traitement_non_attribue() {
    let chaine = maquette_a4();
    let x = ctx(TransportZone::Local, false, -6.0);
    // La sonde voit un traitement ; la RI est connue éteinte (cadence).
    let s = EnService {
        greffons: GreffonsEnService::Sonde {
            actif: true,
            hors_service: vec!["convolution".into()],
        },
        headroom_db: None,
        upsampling: false,
        volume_logiciel: true,
        latences: vec![],
    };
    let c = chemin(&chaine, &x, &s, None);
    assert_eq!(
        greffon(&c, "convolution").etape.reason,
        Some(MotifEtape::NotInService)
    );
    assert!(greffon(&c, "equalizer-graphic").etape.effective);
    // Ordre historique : égaliseur avant convolution quand les deux tournent.
    let s2 = EnService {
        greffons: GreffonsEnService::Sonde {
            actif: true,
            hors_service: vec![],
        },
        ..s.clone()
    };
    let c = chemin(&chaine, &x, &s2, None);
    let ids: Vec<_> = c
        .effectives()
        .filter_map(|e| e.etape.plugin_id.as_deref())
        .collect();
    assert_eq!(ids, ["equalizer-graphic", "convolution"]);
    // Sonde inactive : rien ne tourne, quoi que dise la résolution.
    let s3 = EnService {
        greffons: GreffonsEnService::Sonde {
            actif: false,
            hors_service: vec![],
        },
        volume_logiciel: false,
        ..s.clone()
    };
    let c = chemin(&chaine, &ctx(TransportZone::Local, false, 0.0), &s3, None);
    assert_eq!(c.badge, Badge::BitPerfect);
    // La sonde voit un traitement que la chaîne n'attribue à personne.
    let c = chemin(
        &ChaineTess::default(),
        &ctx(TransportZone::Local, false, 0.0),
        &s2,
        None,
    );
    let dsp = c
        .steps
        .iter()
        .find(|e| e.etape.code == CodeEtape::Plugin)
        .expect("étape non attribuée");
    assert_eq!(dsp.name, "DSP");
    assert!(dsp.etape.effective);
    assert_eq!(dsp.etape.detail.as_deref(), Some(DETAIL_NON_ATTRIBUE));
    assert_eq!(c.badge, Badge::ProcessedByTess);
}

#[test]
fn vers_dsd_le_volume_juste_avant_le_modulateur() {
    let chaine = ChaineTess {
        fixed: EtagesFixes {
            upsampling: Upsampling::Dsd {
                rate: TauxDsd::Dsd128,
                filter: "polyvalent".into(),
            },
            ..EtagesFixes::default()
        },
        ..ChaineTess::default()
    };
    let mut s = service(&[], Some(-3.0), true);
    s.upsampling = true;
    let c = chemin(&chaine, &ctx(TransportZone::Local, false, -6.0), &s, None);
    let codes: Vec<_> = c.effectives().map(|e| e.etape.code).collect();
    assert_eq!(
        codes,
        [
            CodeEtape::Source,
            CodeEtape::Headroom,
            CodeEtape::Upsampling,
            CodeEtape::Volume,
            CodeEtape::DsdModulator,
            CodeEtape::Output
        ]
    );
    assert_eq!(
        c.volume.placement,
        Some(PlacementVolume::BeforeDsdModulator)
    );
    // Upsampling demandé mais pas en service : retour à la quantification PCM.
    s.upsampling = false;
    let c = chemin(&chaine, &ctx(TransportZone::Local, false, -6.0), &s, None);
    assert!(
        c.steps
            .iter()
            .all(|e| e.etape.code != CodeEtape::DsdModulator)
    );
    assert_eq!(c.volume.placement, Some(PlacementVolume::AtRender));
}

#[test]
fn le_niveau_t07_est_publie_sur_headroom_et_volume() {
    let cat = catalogue_de_reference();
    let chaine = maquette_a4();
    let r = chaine.resoudre(&cat, &ctx(TransportZone::Local, false, -6.0));
    let n = NiveauPublie {
        absorbed_by_plugins_db: 2.0,
        unrendered_db: 1.5,
    };
    let c = chemin_du_signal(
        &chaine,
        &cat,
        r,
        &service(&["convolution", "equalizer-graphic"], Some(-1.0), true),
        None,
        Some(n),
    );
    assert_eq!(c.level, Some(n));
    let h = etape(&c, CodeEtape::Headroom).etape.params.clone().unwrap();
    assert_eq!(h["absorbed_by_plugins_db"], 2.0);
    assert_eq!(h["db"], -1.0, "le gain réellement appliqué");
    assert_eq!(c.headroom_db, -1.0);
    let v = etape(&c, CodeEtape::Volume).etape.params.clone().unwrap();
    assert_eq!(v["unrendered_db"], 1.5);
    // JSON : nom d'affichage et champs de l'étape à plat.
    let j = serde_json::to_value(&c).unwrap();
    let conv = j["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["plugin_id"] == "convolution")
        .unwrap();
    assert_eq!(conv["name"], "Convolution");
    assert_eq!(conv["code"], "plugin");
    assert_eq!(j["badge"], "processed_by_tess");
}

#[test]
fn not_in_service_et_altered_outside_tess_sont_au_schema() {
    let s = crate::tess::schema_json().to_string();
    assert!(s.contains("\"not_in_service\""));
    assert!(s.contains("\"altered_outside_tess\""));
}

// ── Test croisé avec de VRAIS étages ──────────────────────────────────
//
// Le banc monte exactement les étages que `EnService` déclare : l'étage
// Headroom et l'étage Volume de T07 (les vrais), et un gain pour chaque
// greffon. Il fait passer un signal 24 bits, requantifie à 24 bits (arrondi
// nu), et compare à la source : le badge « Bit-perfect » doit être publié
// SI ET SEULEMENT SI la sortie est identique à l'octet.

struct Gain(f64, usize);

impl EtageTess for Gain {
    fn id(&self) -> &str {
        "gain-de-test"
    }
    fn preparer(
        &mut self,
        e: FormatFlux,
        max: usize,
    ) -> Result<crate::tess::Disposition, crate::tess::ErreurEtage> {
        self.1 = usize::from(e.channels);
        Ok(crate::tess::Disposition::sur_place(e, max, 0))
    }
    fn traiter_sur_place(
        &mut self,
        t: &mut [f64],
        trames: usize,
    ) -> Result<(), crate::tess::ErreurEtage> {
        for x in &mut t[..trames * self.1] {
            *x *= self.0;
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

fn source_24_bits(trames: usize) -> Vec<i32> {
    (0..trames * 2)
        .map(|i| {
            let x = ((i as f64) * 0.013).sin() * 0.5;
            (x * 8_388_607.0).round() as i32
        })
        .collect()
}

/// Ce que l'hôte ferait réellement de `s` (le volume en dB de la zone).
fn jouer(s: &EnService, volume_db: f64) -> Vec<i32> {
    const TRAMES: usize = 2048;
    let fmt = FormatFlux {
        channels: 2,
        sample_rate_hz: 96_000,
    };
    let mut etages: Vec<Box<dyn EtageTess>> = Vec::new();
    if let Some(db) = s.headroom_db {
        etages.push(Box::new(EtageHeadroom::new(db).unwrap()));
    }
    let n = match &s.greffons {
        GreffonsEnService::Exacts(v) => v.len(),
        GreffonsEnService::Sonde { actif, .. } => usize::from(*actif),
    };
    for _ in 0..n {
        etages.push(Box::new(Gain(lineaire(-0.7), 0)));
    }
    if s.volume_logiciel {
        etages.push(Box::new(EtageVolume::new(
            ConsigneVolume::new(lineaire(volume_db)),
            PlacementVolume::AtRender,
        )));
    }
    let src = source_24_bits(TRAMES);
    let mut t: Vec<f64> = src.iter().map(|&x| f64::from(x) / 8_388_608.0).collect();
    for e in &mut etages {
        e.preparer(fmt, TRAMES).unwrap();
        e.traiter_sur_place(&mut t, TRAMES).unwrap();
    }
    t.iter()
        .map(|x| (x * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32)
        .collect()
}

#[test]
fn croise_avec_de_vrais_etages_bit_perfect_si_et_seulement_si_la_sortie_est_intacte() {
    let chaines = [ChaineTess::default(), maquette_a4()];
    let services = [
        service(&[], None, false),
        service(&[], None, true),
        service(&["equalizer-graphic"], None, false),
        service(&["convolution", "equalizer-graphic"], Some(-3.0), true),
        service(&[], Some(-3.0), false),
        EnService {
            greffons: GreffonsEnService::Sonde {
                actif: true,
                hors_service: vec![],
            },
            ..service(&[], None, false)
        },
        EnService {
            greffons: GreffonsEnService::Sonde {
                actif: false,
                hors_service: vec![],
            },
            ..service(&[], None, false)
        },
    ];
    let mut cas = 0;
    let src = source_24_bits(2048);
    for chaine in &chaines {
        for pure in [false, true] {
            for volume_db in [0.0, -1.0, -18.0] {
                for s in &services {
                    // Le volume logiciel n'est en service que s'il n'est pas
                    // à l'unité : on ne monte pas d'étage à 0 dB.
                    let mut s = s.clone();
                    if volume_db == 0.0 {
                        s.volume_logiciel = false;
                    }
                    let c = chemin(
                        chaine,
                        &ctx(TransportZone::Local, pure, volume_db),
                        &s,
                        Some(true),
                    );
                    let intacte = jouer(&s, volume_db) == src;
                    assert_eq!(
                        c.badge == Badge::BitPerfect,
                        intacte,
                        "pure={pure} volume={volume_db} service={s:?} badge={:?}",
                        c.badge
                    );
                    // Chaque étape effective correspond à un étage monté.
                    for e in c.effectives() {
                        match e.etape.code {
                            CodeEtape::Headroom => assert!(s.headroom_db.is_some()),
                            CodeEtape::Volume => assert!(s.volume_logiciel),
                            CodeEtape::Upsampling => assert!(s.upsampling),
                            CodeEtape::Plugin => match &s.greffons {
                                GreffonsEnService::Exacts(v) => assert!(
                                    v.iter()
                                        .any(|x| Some(x.as_str()) == e.etape.plugin_id.as_deref())
                                ),
                                GreffonsEnService::Sonde { actif, .. } => assert!(*actif),
                            },
                            _ => {}
                        }
                    }
                    cas += 1;
                }
            }
        }
    }
    assert_eq!(cas, 2 * 2 * 3 * services.len());
}

#[test]
fn le_dither_suit_la_chaine() {
    let chaine = ChaineTess {
        fixed: EtagesFixes {
            headroom: Headroom::Auto {},
            quantization: Quantification {
                dither: Dither::NoiseShaped,
            },
            ..EtagesFixes::default()
        },
        ..ChaineTess::default()
    };
    let c = chemin(
        &chaine,
        &ctx(TransportZone::Local, false, -6.0),
        &service(&[], None, true),
        None,
    );
    let q = etape(&c, CodeEtape::Quantization);
    assert!(q.etape.effective);
    assert_eq!(q.etape.params.clone().unwrap()["dither"], "noise_shaped");
}

// ── Reprise T16 : les quatre cas de la mission (chaîne, Bypass all, DSD
// natif, sortie réseau), chacun avec ce qui tourne réellement. ──────────

fn sonde(actif: bool, volume: bool) -> EnService {
    EnService {
        greffons: GreffonsEnService::Sonde {
            actif,
            hors_service: vec![],
        },
        headroom_db: None,
        upsampling: false,
        volume_logiciel: volume,
        latences: vec![],
    }
}

/// Les étapes de traitement (hors source et sortie) et leur motif.
fn motifs(c: &CheminTess) -> Vec<(CodeEtape, Option<&str>, bool, Option<MotifEtape>)> {
    c.steps
        .iter()
        .filter(|e| !matches!(e.etape.code, CodeEtape::Source | CodeEtape::Output))
        .map(|e| {
            (
                e.etape.code,
                e.etape.plugin_id.as_deref(),
                e.etape.effective,
                e.etape.reason,
            )
        })
        .collect()
}

#[test]
fn bypass_all_contourne_greffons_headroom_upsampling_et_garde_le_volume() {
    let mut chaine = maquette_a4();
    chaine.bypass_all = true;
    chaine.fixed.upsampling = Upsampling::Pcm {
        filter: "sinc".into(),
        max_ratio: None,
        max_rate_hz: Some(192_000),
    };
    // L'exécuteur, en « Bypass all », n'appelle aucun étage.
    let s = service(&[], None, true);
    let c = chemin(&chaine, &ctx(TransportZone::Local, false, -12.0), &s, None);
    for (code, id, effective, motif) in motifs(&c) {
        match code {
            CodeEtape::Plugin if id == Some("crossfeed") => {
                // Décoché : rien à contourner.
                assert!(!effective);
                assert_eq!(motif, None);
            }
            CodeEtape::Plugin | CodeEtape::Headroom | CodeEtape::Upsampling => {
                assert!(!effective, "{code:?} {id:?}");
                assert_eq!(motif, Some(MotifEtape::BypassAll), "{code:?} {id:?}");
            }
            // Contrat § 4 : le volume est un réglage de zone, il reste.
            CodeEtape::Volume | CodeEtape::Quantization => assert!(effective, "{code:?}"),
            autre => panic!("étape inattendue {autre:?}"),
        }
    }
    assert_eq!(c.badge, Badge::ProcessedByTess);
    assert_eq!(c.headroom_db, 0.0);
    assert!(c.discrepancies.is_empty(), "{:?}", c.discrepancies);
    // Volume à 0 dB : plus rien ne modifie, « Bit-perfect ».
    let c = chemin(
        &chaine,
        &ctx(TransportZone::Local, false, 0.0),
        &service(&[], None, false),
        None,
    );
    assert_eq!(c.badge, Badge::BitPerfect);
    assert_eq!(noms_effectifs(&c), ["Source", "Output"]);
    // Mais si un moteur traite malgré le « Bypass all », c'est publié, et
    // le badge ne dit pas « Bit-perfect ».
    let c = chemin(
        &chaine,
        &ctx(TransportZone::Local, false, 0.0),
        &sonde(true, false),
        None,
    );
    assert_eq!(c.badge, Badge::ProcessedByTess);
    assert!(
        c.discrepancies
            .iter()
            .any(|d| d.kind == SensEcart::RunningNotRequested),
        "{:?}",
        c.discrepancies
    );
    assert_eq!(
        greffon(&c, "equalizer-graphic").etape.reason,
        Some(MotifEtape::BypassAll)
    );
}

#[test]
fn dsd_natif_rien_n_est_traite_et_chaque_etape_dit_pourquoi() {
    let mut chaine = maquette_a4();
    chaine.fixed.headroom = Headroom::Manual { db: -3.0 };
    let mut x = ctx(TransportZone::Local, false, -12.0);
    x.source = Some(SourceFlux::Dsd {
        rate: TauxDsd::Dsd128,
        channels: 2,
        played_as: crate::tess::resolution::LectureDsd::Native,
    });
    // La sortie native ne peut ni filtrer ni atténuer un flux DSD.
    let c = chemin(&chaine, &x, &service(&[], None, false), Some(true));
    for (code, id, effective, motif) in motifs(&c) {
        assert!(!effective, "{code:?} {id:?}");
        match code {
            CodeEtape::Plugin if id == Some("crossfeed") => assert_eq!(motif, None),
            CodeEtape::Plugin | CodeEtape::Headroom | CodeEtape::Volume => {
                assert_eq!(motif, Some(MotifEtape::DsdSourceNative), "{code:?} {id:?}")
            }
            CodeEtape::Upsampling => assert_eq!(motif, None, "rien de demandé"),
            CodeEtape::Quantization => assert_eq!(motif, Some(MotifEtape::NothingToQuantize)),
            autre => panic!("étape inattendue {autre:?}"),
        }
    }
    assert_eq!(c.badge, Badge::BitPerfect);
    assert_eq!(c.volume.placement, None);
    let src = etape(&c, CodeEtape::Source).etape.params.clone().unwrap();
    assert_eq!(src["kind"], "dsd");
    assert_eq!(src["played_as"], "native");
    assert!(c.discrepancies.is_empty(), "{:?}", c.discrepancies);
}

#[test]
fn sortie_reseau_volume_de_l_appareil_ordre_reel_et_repli_mono_inerte() {
    let mut chaine = maquette_a4();
    // D2 : le repli mono hérité est décoché d'office sur le réseau.
    chaine.plugins.push(maillon(
        "mono-downmix",
        false,
        json!({ MARQUE_MONO_INERTE: true }),
    ));
    let mut x = ctx(TransportZone::Dlna, false, 0.0);
    x.volume = VolumeZone {
        control: ControleVolume::Device,
        db: 0.0,
    };
    // Le flux réseau est traité (StreamingDsp) dans l'ordre historique.
    let c = chemin(&chaine, &x, &sonde(true, false), Some(false));
    let ids: Vec<_> = c
        .effectives()
        .filter_map(|e| e.etape.plugin_id.as_deref())
        .collect();
    assert_eq!(ids, ["equalizer-graphic", "convolution"]);
    let vol = etape(&c, CodeEtape::Volume);
    assert!(!vol.etape.effective);
    assert_eq!(vol.etape.reason, Some(MotifEtape::VolumeNotSoftware));
    assert_eq!(c.volume.control, ControleVolume::Device);
    assert_eq!(c.volume.placement, None);
    let mono = greffon(&c, "mono-downmix");
    assert!(!mono.etape.effective);
    assert_eq!(mono.etape.detail.as_deref(), Some(DETAIL_INERTE_RESEAU));
    assert_eq!(mono.name, "Mono downmix");
    assert_eq!(c.badge, Badge::ProcessedByTess);
    // Rien ne traite le flux : bit-perfect côté TESS, mais un chemin altéré
    // ailleurs (transcodage du transport) ne l'est jamais.
    let c = chemin(&chaine, &x, &sonde(false, false), Some(false));
    assert_eq!(c.badge, Badge::AlteredOutsideTess);
    let c = chemin(&chaine, &x, &sonde(false, false), Some(true));
    assert_eq!(c.badge, Badge::BitPerfect);
}
