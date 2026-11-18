//! Épreuves du lot T11a (plan TESS, ligne T11a). Banc factice : AUCUN son
//! réel, aucun périphérique.
//!
//! - reprise de TOUS les réglages de #7614 (filtre, `max_ratio`,
//!   `max_rate_hz`) et test de la machine conservé ;
//! - 64 bits réels (aucun passage par `f32`) ;
//! - non-régression des six filtres à ×2, ×4, ×8 (44,1 et 48 kHz) contre le
//!   greffon actuel ;
//! - la réserve de 3 dB a disparu (le headroom de T07 la remplace) ;
//! - changement de filtre entendu au bloc suivant, sans attendre la piste
//!   suivante ; contre-épreuve : même filtre, rien ne bouge ;
//! - mise en forme du bruit à la cadence suréchantillonnée ;
//! - bit-perfect strict (Pure) toujours refusé, avec motif ;
//! - mesure CPU (`#[ignore]`, lancée à la main sur Shrek).

use serde_json::json;
use tune_plugin_sdk::audio::{
    AudioBlock, AudioFormat, BlockContext, ChannelLayout, SampleEncoding, SamplesMut,
};
use tune_plugin_sdk::rate::{RateChanger, RateRequest};
use tune_plugin_surechantillonnage::conception::par_id;
use tune_plugin_surechantillonnage::polyphase::Surechantillonneur;
use tune_plugin_surechantillonnage::sdk::{Instance, OFFERED_FILTERS, UpsamplerSettings};

use super::*;
use crate::audio::surechantillonnage::{EtageSurechantillonneur, MesureMachine, VerdictMachine};
use crate::tess::chaine::{ChaineTess, Headroom, Upsampling};
use crate::tess::etage::{EtapeFinale, preparer_chaine};
use crate::tess::etages_fixes::{BilanDeNiveau, EntreesNiveau, QuantificationMiseEnForme};
use crate::tess::greffon::catalogue_de_reference;
use crate::tess::resolution::{
    Badge, CodeEtape, ContexteExecution, ControleVolume, MotifEtape, SourceFlux, VolumeZone,
};

const BLOC: usize = 512;

fn format(cadence: u32) -> FormatFlux {
    FormatFlux {
        channels: 2,
        sample_rate_hz: cadence,
    }
}

/// Signal de musique factice, stéréo, crête ≤ `amplitude` : deux sinus dont
/// un près de Nyquist (là où les filtres diffèrent le plus), voies décalées.
fn signal(trames: usize, cadence: u32, amplitude: f64) -> Vec<f64> {
    let sr = f64::from(cadence);
    let mut v = Vec::with_capacity(trames * 2);
    for i in 0..trames {
        let t = i as f64 / sr;
        let x = 0.6 * (2.0 * std::f64::consts::PI * 997.0 * t).sin()
            + 0.4 * (2.0 * std::f64::consts::PI * 0.45 * sr * t).sin();
        let y = 0.6 * (2.0 * std::f64::consts::PI * 1_499.0 * t + 0.3).sin()
            + 0.4 * (2.0 * std::f64::consts::PI * 0.43 * sr * t).sin();
        v.push(amplitude * x);
        v.push(amplitude * y);
    }
    v
}

/// Fait passer `entree` par l'étage, bloc par bloc, et rend toute la sortie.
fn jouer(etage: &mut EtageUpsampling, de: u32, entree: &[f64], bloc: usize) -> Vec<f64> {
    let d = etage.preparer(format(de), bloc).unwrap();
    let mut sortie = vec![0.0; d.capacite_sortie()];
    let mut tout = Vec::new();
    for morceau in entree.chunks(bloc * 2) {
        let n = etage
            .traiter_vers(morceau, morceau.len() / 2, &mut sortie)
            .unwrap();
        tout.extend_from_slice(&sortie[..n * 2]);
    }
    tout
}

/// Le calcul de référence : l'interpolateur du SDK, seul, en `f64`.
fn reference(filtre: &str, de: u32, vers: u32, entree: &[f64]) -> Vec<f64> {
    let mut s = Surechantillonneur::nouveau(&par_id(filtre).unwrap(), de, vers, 2).unwrap();
    let mut sortie = Vec::new();
    s.traiter(entree, &mut sortie);
    sortie
}

fn verdict_admettant(filtres: &[&str]) -> VerdictMachine {
    let mut results = Vec::new();
    for f in filtres {
        for (de, vers) in [
            (44_100, 352_800),
            (44_100, 705_600),
            (48_000, 384_000),
            (48_000, 768_000),
        ] {
            results.push(MesureMachine {
                filter: (*f).to_string(),
                from_hz: de,
                to_hz: vers,
                realtime_factor: 10.0,
                admitted: true,
            });
        }
    }
    VerdictMachine {
        simd_path: "test".into(),
        margin: 3.0,
        results,
        measured_at: 0,
    }
}

// --- Réglages (contrat B3) ------------------------------------------------

#[test]
fn tous_les_reglages_de_7614_sont_repris_aller_retour() {
    let mut vus = 0;
    for f in OFFERED_FILTERS {
        for ratio in [None, Some(2), Some(4), Some(8), Some(16)] {
            for plafond in [None, Some(1), Some(192_000), Some(768_000)] {
                let greffon = UpsamplerSettings {
                    enabled: true,
                    filter: f.to_string(),
                    max_ratio: ratio,
                    max_rate_hz: plafond,
                };
                let r = ReglagesUpsamplingPcm::depuis_greffon(&greffon).expect("réglage valide");
                assert_eq!(r.vers_greffon(), greffon, "greffon → TESS → greffon");
                let chaine = r.vers_chaine();
                assert_eq!(
                    ReglagesUpsamplingPcm::depuis_chaine(&chaine),
                    Ok(Some(r)),
                    "TESS → chaîne → TESS"
                );
                // Le contrat T01 accepte exactement la même valeur.
                let c = ChaineTess {
                    fixed: crate::tess::chaine::EtagesFixes {
                        upsampling: chaine,
                        ..Default::default()
                    },
                    ..ChaineTess::default()
                };
                assert!(
                    c.valider(
                        &catalogue_de_reference(),
                        crate::tess::chaine::ContexteValidation { premium: true }
                    )
                    .is_ok()
                );
                vus += 1;
            }
        }
    }
    assert_eq!(vus, 6 * 5 * 4);
}

#[test]
fn reglages_refuses_ou_eteints_avec_motif() {
    let off = UpsamplerSettings::default();
    assert_eq!(ReglagesUpsamplingPcm::depuis_greffon(&off), None);
    for (filtre, ratio, plafond) in [
        ("inconnu", None, None),
        ("polyvalent", Some(3), None),
        ("polyvalent", Some(32), None),
        ("polyvalent", None, Some(0)),
        ("polyvalent", None, Some(768_001)),
        ("precision-longue", None, None),
    ] {
        let u = Upsampling::Pcm {
            filter: filtre.into(),
            max_ratio: ratio,
            max_rate_hz: plafond,
        };
        assert_eq!(
            ReglagesUpsamplingPcm::depuis_chaine(&u),
            Err(MotifUpsampling::ReglagesInvalides),
            "{u:?}"
        );
    }
    assert_eq!(
        ReglagesUpsamplingPcm::depuis_chaine(&Upsampling::Off {}),
        Ok(None)
    );
    assert_eq!(
        ReglagesUpsamplingPcm::depuis_chaine(&Upsampling::Unknown),
        Err(MotifUpsampling::ValeurInconnue)
    );
    let dsd: Upsampling = serde_json::from_value(json!({"mode":"dsd","rate":"dsd128"})).unwrap();
    assert_eq!(
        ReglagesUpsamplingPcm::depuis_chaine(&dsd),
        Err(MotifUpsampling::VersDsd)
    );
}

#[test]
fn negociation_plafonds_et_test_de_la_machine_conserves() {
    let r = |f: &'static str, max_ratio, max_rate_hz| ReglagesUpsamplingPcm {
        filtre: f,
        max_ratio,
        max_rate_hz,
    };
    let toutes = [
        88_200, 176_400, 352_800, 705_600, 96_000, 192_000, 384_000, 768_000,
    ];
    let v = verdict_admettant(&["polyvalent"]);
    // Maximum du périphérique.
    assert_eq!(
        negocier(&r("polyvalent", None, None), 44_100, &toutes, Some(&v)),
        Ok(705_600)
    );
    // max_ratio et max_rate_hz, chacun et ensemble.
    assert_eq!(
        negocier(&r("polyvalent", Some(2), None), 44_100, &toutes, Some(&v)),
        Ok(88_200)
    );
    assert_eq!(
        negocier(&r("polyvalent", Some(4), None), 48_000, &toutes, Some(&v)),
        Ok(192_000)
    );
    assert_eq!(
        negocier(
            &r("polyvalent", None, Some(192_000)),
            44_100,
            &toutes,
            Some(&v)
        ),
        Ok(176_400)
    );
    assert_eq!(
        negocier(
            &r("polyvalent", Some(8), Some(192_000)),
            48_000,
            &toutes,
            Some(&v)
        ),
        Ok(192_000)
    );
    // Test de la machine : absent, ou filtre non admis.
    assert_eq!(
        negocier(&r("polyvalent", None, None), 44_100, &toutes, None),
        Err(MotifUpsampling::TestDeLaMachineAbsent)
    );
    assert_eq!(
        negocier(&r("precision", None, None), 44_100, &toutes, Some(&v)),
        Err(MotifUpsampling::FiltreNonAdmis)
    );
    // #8149 / #8259 : la sortie n'accepte que la famille de l'autre source.
    assert_eq!(
        negocier(
            &r("polyvalent", None, None),
            44_100,
            &[96_000, 192_000],
            Some(&v)
        ),
        Err(MotifUpsampling::AucuneCadence)
    );
    assert_eq!(
        MotifUpsampling::AucuneCadence.code(),
        "no_integer_rate_available"
    );
}

#[test]
fn transport_local_seulement_pour_l_instant() {
    assert_eq!(transport_porte_l_upsampling(TransportZone::Local), Ok(()));
    for t in [
        TransportZone::Oaat,
        TransportZone::Dlna,
        TransportZone::AirPlay,
    ] {
        assert_eq!(
            transport_porte_l_upsampling(t),
            Err(MotifUpsampling::TransportEnAttente)
        );
    }
}

#[test]
fn planifier_le_changement() {
    let a = ReglagesUpsamplingPcm {
        filtre: "polyvalent",
        max_ratio: None,
        max_rate_hz: None,
    };
    let b = ReglagesUpsamplingPcm {
        filtre: "resolution-pm",
        ..a
    };
    let a2 = ReglagesUpsamplingPcm {
        max_ratio: Some(16),
        ..a
    };
    use ChangementUpsampling::*;
    assert_eq!(planifier_changement(None, None), Aucun);
    assert_eq!(
        planifier_changement(Some((&a, 176_400)), Some((&a2, 176_400))),
        Aucun
    );
    assert_eq!(
        planifier_changement(Some((&a, 176_400)), Some((&b, 176_400))),
        FiltreAChaud {
            filtre: "resolution-pm"
        }
    );
    assert_eq!(
        planifier_changement(Some((&a, 176_400)), Some((&a, 352_800))),
        RouvrirLaSortie
    );
    assert_eq!(
        planifier_changement(Some((&a, 176_400)), None),
        RouvrirLaSortie
    );
    assert_eq!(
        planifier_changement(None, Some((&a, 176_400))),
        RouvrirLaSortie
    );
}

// --- 64 bits réels -----------------------------------------------------------

#[test]
fn soixante_quatre_bits_reels_de_bout_en_bout() {
    let entree = signal(4_000, 44_100, 0.5);
    let mut etage = EtageUpsampling::nouveau("polyvalent", 176_400).unwrap();
    let sortie = jouer(&mut etage, 44_100, &entree, BLOC);
    // Identique, au bit près, à l'interpolateur f64 du SDK : aucun gain,
    // aucune conversion.
    assert_eq!(sortie, reference("polyvalent", 44_100, 176_400, &entree));
    let hors_f32 = sortie.iter().filter(|x| (**x as f32) as f64 != **x).count();
    assert!(hors_f32 > sortie.len() / 2, "{hors_f32} valeurs hors f32");
    // Contre-épreuve à l'entrée : une différence sous la précision de f32
    // doit se retrouver en sortie.
    let mut entree2 = entree.clone();
    entree2[2 * 1_000] += 2f64.powi(-40);
    assert_eq!(entree2[2000] as f32, entree[2000] as f32);
    let mut etage2 = EtageUpsampling::nouveau("polyvalent", 176_400).unwrap();
    let sortie2 = jouer(&mut etage2, 44_100, &entree2, BLOC);
    assert_ne!(
        sortie, sortie2,
        "une entrée passée par f32 aurait tout effacé"
    );
}

// --- Non-régression des filtres actuels -----------------------------------

fn instance_actuelle_f64(filtre: &str, de: u32, vers: u32, entree: &[f64]) -> (Vec<f64>, f64) {
    let fmt = AudioFormat::new(de, ChannelLayout::Stereo, SampleEncoding::F64).unwrap();
    let req = RateRequest::new(fmt, vers, BLOC).unwrap();
    let mut inst = Instance::new(req, filtre).unwrap();
    let mut tout = Vec::new();
    let mut tampon_sortie = vec![0.0f64; req.max_output_frames() * 2];
    for morceau in entree.chunks(BLOC * 2) {
        let mut e = morceau.to_vec();
        let trames = e.len() / 2;
        let bloc_e = AudioBlock::new(fmt, SamplesMut::F64(&mut e), BLOC).unwrap();
        let cap = tampon_sortie.len() / 2;
        let mut bloc_s = AudioBlock::new(
            req.output_format(),
            SamplesMut::F64(&mut tampon_sortie),
            cap,
        )
        .unwrap();
        let r = inst
            .process(
                &bloc_e,
                &mut bloc_s,
                BlockContext {
                    zone_id: 0,
                    generation: 0,
                    position_frames: 0,
                },
            )
            .unwrap();
        assert_eq!(r.frames_consumed, trames);
        tout.extend_from_slice(&tampon_sortie[..r.frames_written * 2]);
    }
    (tout, inst.gain())
}

#[test]
fn non_regression_des_six_filtres_a_x2_x4_x8() {
    for de in [44_100u32, 48_000] {
        let entree = signal(1_500, de, 0.5);
        for filtre in OFFERED_FILTERS {
            for rapport in [2u32, 4, 8] {
                let vers = de * rapport;
                let mut etage = EtageUpsampling::nouveau(filtre, vers).unwrap();
                let tess = jouer(&mut etage, de, &entree, BLOC);
                assert_eq!(etage.compteur_du_limiteur().trames_limitees, 0);
                // Le greffon actuel, en f64 : notre sortie × sa réserve, au
                // bit près (le limiteur est inerte à ce niveau).
                let (actuel, g) = instance_actuelle_f64(filtre, de, vers, &entree);
                assert!(g < 1.0 && g > 0.7, "réserve actuelle {g}");
                let tess_g: Vec<f64> = tess.iter().map(|x| x * g).collect();
                assert_eq!(tess_g, actuel, "{filtre} {de}→{vers}");
                // L'étage de production (f32) : à la précision de f32 près.
                let s = UpsamplerSettings {
                    enabled: true,
                    filter: filtre.to_string(),
                    ..Default::default()
                };
                let mut prod = EtageSurechantillonneur::nouveau(&s, de, vers, 2).unwrap();
                let e32: Vec<f32> = entree.iter().map(|x| *x as f32).collect();
                let p = prod.traiter(&e32);
                assert_eq!(p.len(), tess.len());
                let ecart = p
                    .iter()
                    .zip(&tess_g)
                    .map(|(a, b)| (f64::from(*a) - b).abs())
                    .fold(0.0, f64::max);
                assert!(ecart < 1e-6, "{filtre} {de}→{vers} : écart {ecart}");
            }
        }
    }
}

// --- La réserve de 3 dB disparaît (headroom de T07) ------------------------

#[test]
fn plus_de_reserve_de_3_db_le_headroom_de_t07_la_remplace() {
    let entree = vec![0.5; 2 * 6_000];
    let mut etage = EtageUpsampling::nouveau("precision", 192_000).unwrap();
    let sortie = jouer(&mut etage, 48_000, &entree, BLOC);
    let fin = sortie[sortie.len() - 2];
    assert!((fin - 0.5).abs() < 1e-9, "continu rendu à {fin}, pas 0,5");
    // Ce que faisait l'ancien étage : −3 dB.
    let (actuel, _) = instance_actuelle_f64("precision", 48_000, 192_000, &entree);
    let fin_actuel = actuel[actuel.len() - 2];
    assert!((20.0 * (fin_actuel / 0.5).log10() + 3.0).abs() < 0.01);

    // Sous TESS : l'upsampling seul arme le headroom auto à −3 dB, une fois.
    let chaine = ChaineTess {
        fixed: crate::tess::chaine::EtagesFixes {
            upsampling: Upsampling::Pcm {
                filter: "precision".into(),
                max_ratio: None,
                max_rate_hz: None,
            },
            ..Default::default()
        },
        ..ChaineTess::default()
    };
    let r = chaine.resoudre(&catalogue_de_reference(), &ctx(false));
    assert_eq!(r.headroom_db, -3.0);
    let bilan = BilanDeNiveau::calculer(&EntreesNiveau {
        headroom: Headroom::Auto {},
        headroom_resolu_db: r.headroom_db,
        replaygain_db: 0.0,
        gain_de_zone_db: 0.0,
        gain_moyen_greffons_db: 0.0,
        compensation_active: false,
    });
    // −3 dB à l'entrée, 0 dB dans l'étage : −3 dB en tout, et non −6 dB.
    assert_eq!(bilan.gain_d_entree_db, -3.0);
}

fn installe(_: &str) -> bool {
    true
}

fn ctx(pure: bool) -> ContexteExecution<'static> {
    ContexteExecution {
        transport: TransportZone::Local,
        source: Some(SourceFlux::Pcm {
            sample_rate_hz: 44_100,
            bits: 24,
            channels: 2,
        }),
        premium: true,
        greffon_disponible: &installe,
        pure,
        volume: VolumeZone {
            control: ControleVolume::Software,
            db: 0.0,
        },
    }
}

// --- Changement à chaud ------------------------------------------------------

/// Banc factice : on change de filtre au milieu d'un flux, entre deux blocs.
fn changer_en_cours(
    avant: &str,
    apres: &str,
    bloc_du_changement: usize,
) -> (Vec<f64>, EtageUpsampling) {
    let entree = signal(BLOC * 40, 44_100, 0.5);
    let mut etage = EtageUpsampling::nouveau(avant, 176_400).unwrap();
    let d = etage.preparer(format(44_100), BLOC).unwrap();
    let mut sortie = vec![0.0; d.capacite_sortie()];
    let mut tout = Vec::new();
    for (k, morceau) in entree.chunks(BLOC * 2).enumerate() {
        if k == bloc_du_changement {
            etage
                .appliquer_reglages(&json!({"mode":"pcm","filter":apres}))
                .unwrap();
        }
        let n = etage.traiter_vers(morceau, BLOC, &mut sortie).unwrap();
        assert_eq!(n, BLOC * 4, "le flux ne s'interrompt pas");
        tout.extend_from_slice(&sortie[..n * 2]);
    }
    (tout, etage)
}

#[test]
fn changement_de_filtre_entendu_au_bloc_suivant_sans_attendre_la_piste() {
    let k = 10;
    let (tout, etage) = changer_en_cours("polyvalent", "resolution-pm", k);
    let entree = signal(BLOC * 40, 44_100, 0.5);
    let ref_a = reference("polyvalent", 44_100, 176_400, &entree);
    let ref_b = reference("resolution-pm", 44_100, 176_400, &entree);
    let debut = k * BLOC * 4 * 2; // premier échantillon après le changement
    let fondu = (176_400 * RAMPE_MS as usize / 1000) * 2;
    // Avant : l'ancien filtre, au bit près.
    assert_eq!(tout[..debut], ref_a[..debut]);
    // Le changement s'entend dès le bloc qui suit l'appel…
    let premier = (debut..debut + BLOC * 8)
        .find(|&i| tout[i] != ref_a[i])
        .expect("aucun effet dans le bloc suivant");
    let ms = (premier - debut) as f64 / 2.0 / 176.4;
    assert!(ms < 1.0, "premier effet à {ms} ms");
    // … et après le fondu de 20 ms, c'est le nouveau filtre AU BIT PRÈS,
    // comme s'il avait joué depuis le début (amorce sur l'historique).
    let apres = debut + fondu;
    assert_eq!(tout[apres..], ref_b[apres..]);
    assert!(!etage.fondu_en_cours());
    assert_eq!(etage.filtre(), "resolution-pm");
    // Sans clic : la plus forte marche pendant le fondu ne dépasse pas celle
    // du signal hors fondu.
    let marche = |s: &[f64]| {
        s.chunks_exact(2)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs().max((w[1][1] - w[0][1]).abs()))
            .fold(0.0, f64::max)
    };
    let pendant = marche(&tout[debut..apres + 2]);
    let hors = marche(&ref_a[..debut]).max(marche(&ref_b[apres..]));
    assert!(pendant <= hors * 1.05, "marche {pendant} > {hors}");
}

#[test]
fn contre_epreuve_meme_filtre_rien_ne_bouge() {
    let (tout, etage) = changer_en_cours("polyvalent", "polyvalent", 10);
    let entree = signal(BLOC * 40, 44_100, 0.5);
    assert_eq!(tout, reference("polyvalent", 44_100, 176_400, &entree));
    assert!(!etage.fondu_en_cours());
}

#[test]
fn changement_de_cadence_demande_la_reconstruction() {
    let mut etage = EtageUpsampling::nouveau("polyvalent", 176_400).unwrap();
    etage.preparer(format(44_100), BLOC).unwrap();
    // ×2 au plus : la cadence ouverte (×4) ne tient plus.
    assert_eq!(
        etage.appliquer_reglages(&json!({"mode":"pcm","filter":"polyvalent","max_ratio":2})),
        Err(ErreurEtage::ReconstructionRequise)
    );
    assert_eq!(
        etage.appliquer_reglages(&json!({"mode":"off"})),
        Err(ErreurEtage::ReconstructionRequise)
    );
    assert_eq!(
        etage.appliquer_reglages(&json!({"mode":"dsd","rate":"dsd64"})),
        Err(ErreurEtage::ReconstructionRequise)
    );
    assert_eq!(
        etage.appliquer_reglages(&json!({"mode":"pcm","filter":"x"})),
        Err(ErreurEtage::Reglages("invalid_upsampling"))
    );
    // Plafond qui garde la cadence ouverte : accepté à chaud.
    assert_eq!(
        etage.appliquer_reglages(&json!({"mode":"pcm","filter":"precision","max_rate_hz":192_000})),
        Ok(())
    );
    assert!(etage.fondu_en_cours());
}

#[test]
fn cadence_non_entiere_refusee_a_la_preparation() {
    let mut etage = EtageUpsampling::nouveau("polyvalent", 96_000).unwrap();
    assert!(matches!(
        etage.preparer(format(44_100), BLOC),
        Err(ErreurEtage::FormatNonPrisEnCharge { .. })
    ));
    let mut etage = EtageUpsampling::nouveau("polyvalent", 44_100 * 32).unwrap();
    assert!(etage.preparer(format(44_100), BLOC).is_err());
    assert!(EtageUpsampling::nouveau("precision-longue", 176_400).is_err());
}

// --- Vidange, réinitialisation, temps réel --------------------------------

#[test]
fn vidange_rend_exactement_la_latence_et_reinitialiser_repart_a_neuf() {
    let entree = signal(3_000, 48_000, 0.5);
    let mut etage = EtageUpsampling::nouveau("precision", 384_000).unwrap();
    let d = etage.preparer(format(48_000), BLOC).unwrap();
    assert_eq!(d.latence_trames, etage.latence_trames());
    assert!(!d.sur_place);
    assert_eq!(d.sortie, format(384_000));
    assert_eq!(d.max_trames_sortie, BLOC * 8);
    let premier = jouer(&mut etage, 48_000, &entree, BLOC);
    let mut queue = vec![0.0; 2 * 300];
    let mut vidange = Vec::new();
    loop {
        let n = etage.vidanger(&mut queue).unwrap();
        if n == 0 {
            break;
        }
        vidange.extend_from_slice(&queue[..n * 2]);
    }
    assert_eq!(vidange.len() / 2, d.latence_trames as usize);
    let mut avec_zeros = entree.clone();
    avec_zeros.extend(std::iter::repeat_n(0.0, 2 * 3_000));
    let r = reference("precision", 48_000, 384_000, &avec_zeros);
    assert_eq!(vidange[..], r[premier.len()..premier.len() + vidange.len()]);

    // Réinitialiser (sans repréparer) : la même entrée redonne la même
    // sortie, au bit près.
    etage.reinitialiser();
    let mut s = vec![0.0; d.capacite_sortie()];
    let mut deuxieme = Vec::new();
    for m in entree.chunks(BLOC * 2) {
        let n = etage.traiter_vers(m, m.len() / 2, &mut s).unwrap();
        deuxieme.extend_from_slice(&s[..n * 2]);
    }
    assert_eq!(deuxieme, premier);
}

#[test]
fn aucune_allocation_au_traitement_ni_au_fondu() {
    let entree = signal(BLOC * 30, 44_100, 0.5);
    let mut etage = EtageUpsampling::nouveau("polyvalent", 352_800).unwrap();
    let d = etage.preparer(format(44_100), BLOC).unwrap();
    let mut sortie = vec![0.0; d.capacite_sortie()];
    etage.changer_de_filtre("precision-pm").unwrap();
    let capacites = |e: &EtageUpsampling| {
        (
            e.sortie_moteur.capacity(),
            e.sortie_suivant.capacity(),
            e.entree_propre.capacity(),
            e.historique.capacity(),
        )
    };
    let avant = capacites(&etage);
    for m in entree.chunks(BLOC * 2) {
        etage.traiter_vers(m, BLOC, &mut sortie).unwrap();
        // Les tampons ne grandissent jamais (seul un échange de Vec au fondu).
        let c = capacites(&etage);
        assert!(c.0 == avant.0 || c.0 == avant.1, "{c:?} {avant:?}");
        assert_eq!((c.2, c.3), (avant.2, avant.3));
    }
    assert!(!etage.fondu_en_cours());
    assert!(
        etage.remplace.is_some(),
        "l'ancien filtre attend le hors temps réel"
    );
    etage.changer_de_filtre("polyvalent").unwrap();
    assert!(etage.remplace.is_none());
    // Une entrée non finie n'empoisonne pas le filtre.
    let mut sale = entree[..BLOC * 2].to_vec();
    sale[3] = f64::NAN;
    let n = etage.traiter_vers(&sale, BLOC, &mut sortie).unwrap();
    assert!(sortie[..n * 2].iter().all(|x| x.is_finite()));
}

// --- Mise en forme du bruit à la cadence de sortie -------------------------

#[test]
fn mise_en_forme_du_bruit_a_la_cadence_suréchantillonnee() {
    let mut etage = EtageUpsampling::nouveau("polyvalent", 176_400).unwrap();
    let mut etages: [&mut dyn EtageTess; 1] = [&mut etage];
    let plan = preparer_chaine(&mut etages, format(44_100), BLOC).unwrap();
    assert_eq!(plan.sortie, format(176_400));
    let mut q = QuantificationMiseEnForme::new();
    let cap = q.preparer(plan.sortie, plan.max_trames_sortie).unwrap();
    assert_eq!(
        q.formeur(),
        "ordre2",
        "formeur choisi à la cadence de SORTIE"
    );
    // Contre-épreuve : préparé (à tort) à la cadence source, le formeur
    // serait celui de 44,1 kHz.
    let mut q_source = QuantificationMiseEnForme::new();
    q_source.preparer(format(44_100), BLOC).unwrap();
    assert_eq!(q_source.formeur(), "lipshitz5e");
    // La chaîne complète tourne : upsampling puis quantification 24 bits.
    let entree = signal(BLOC, 44_100, 0.5);
    let mut s = vec![0.0; plan.dispositions[0].capacite_sortie()];
    let n = etage.traiter_vers(&entree, BLOC, &mut s).unwrap();
    let mut mots = vec![0i32; cap];
    let ecrits = q
        .finaliser(
            &s[..n * 2],
            n,
            crate::tess::etage::SortieFinale::Pcm {
                echantillons: &mut mots,
                bits: 24,
            },
        )
        .unwrap();
    assert_eq!(ecrits, BLOC * 4 * 2);
}

// --- Bit-perfect strict toujours refusé -------------------------------------

#[test]
fn bit_perfect_strict_refuse_l_upsampling_avec_motif() {
    let chaine = ChaineTess {
        fixed: crate::tess::chaine::EtagesFixes {
            upsampling: Upsampling::Pcm {
                filter: "polyvalent".into(),
                max_ratio: None,
                max_rate_hz: None,
            },
            ..Default::default()
        },
        ..ChaineTess::default()
    };
    let r = chaine.resoudre(&catalogue_de_reference(), &ctx(true));
    let up = r
        .etapes
        .iter()
        .find(|e| e.code == CodeEtape::Upsampling)
        .unwrap();
    assert!(up.requested && !up.effective);
    assert_eq!(up.reason, Some(MotifEtape::PureMode));
    assert_eq!(r.badge, Badge::BitPerfect);
    // Contre-épreuve : hors Pure, l'étape est effective.
    let r = chaine.resoudre(&catalogue_de_reference(), &ctx(false));
    let up = r
        .etapes
        .iter()
        .find(|e| e.code == CodeEtape::Upsampling)
        .unwrap();
    assert!(up.effective);
}

// --- CPU (à la main sur Shrek) ----------------------------------------------

/// `cargo test -p tune-core --lib -- --ignored --nocapture banc_cpu_upsampling_t11a`
#[test]
#[ignore = "banc CPU, lancé à la main"]
fn banc_cpu_upsampling_t11a() {
    println!(
        "chemin SIMD : {}",
        tune_plugin_surechantillonnage::chemin_simd().nom()
    );
    println!("filtre;de;vers;x;facteur_temps_reel");
    for de in [44_100u32, 48_000] {
        let secondes = 5usize;
        let entree = signal(de as usize * secondes, de, 0.5);
        for filtre in OFFERED_FILTERS {
            for rapport in [2u32, 4, 8] {
                let mut etage = EtageUpsampling::nouveau(filtre, de * rapport).unwrap();
                let t = std::time::Instant::now();
                let s = jouer(&mut etage, de, &entree, 1_024);
                let dt = t.elapsed().as_secs_f64();
                std::hint::black_box(&s);
                println!(
                    "{filtre};{de};{};x{rapport};{:.1}",
                    de * rapport,
                    secondes as f64 / dt
                );
            }
        }
    }
}
