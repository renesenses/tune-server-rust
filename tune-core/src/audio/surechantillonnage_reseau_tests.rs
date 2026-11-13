//! Discussion #8519 — la partie pure de la décision réseau, et l'étage
//! appliqué à une piste entière de PCM.
use super::*;
use crate::audio::surechantillonnage::{EtageSurechantillonneur, MesureMachine, VerdictMachine};

fn sink(entrees: &[&str]) -> Vec<String> {
    entrees.iter().map(|e| e.to_string()).collect()
}

fn choix(admis: bool, reglages: UpsamplerSettings) -> ChoixSurechantillonnage {
    let mesure = |de, vers| MesureMachine {
        filter: "polyvalent".into(),
        from_hz: de,
        to_hz: vers,
        realtime_factor: 10.0,
        admitted: admis,
    };
    ChoixSurechantillonnage {
        reglages,
        verdict: VerdictMachine {
            simd_path: "test".into(),
            margin: 3.0,
            results: vec![
                mesure(44_100, 352_800),
                mesure(44_100, 705_600),
                mesure(48_000, 384_000),
                mesure(48_000, 768_000),
            ],
            measured_at: 0,
        },
    }
}

fn actif() -> UpsamplerSettings {
    UpsamplerSettings {
        enabled: true,
        ..Default::default()
    }
}

#[test]
fn les_cadences_declarees_sont_lues_pour_le_format_servi_8519() {
    let s = sink(&[
        "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM",
        "http-get:*:audio/L24;rate=192000;channels=2:*",
        "http-get:*:audio/x-flac;rate=176400:*",
        "http-get:*:audio/flac;rate=192000;channels=2:*",
        "http-get:*:audio/mpeg:*",
    ]);
    assert_eq!(cadences_declarees(&s, "audio/flac"), vec![176_400, 192_000]);
    assert_eq!(cadences_declarees(&s, "audio/wav"), vec![44_100, 192_000]);
    assert_eq!(
        cadences_declarees_pcm(&s),
        vec![44_100, 176_400, 192_000],
        "toutes les entrées PCM, sans le MP3"
    );
    // Le cas courant : un FLAC sans `rate=` ne déclare rien.
    let muet = sink(&["http-get:*:audio/flac:*", "http-get:*:audio/wav:*"]);
    assert!(cadences_declarees(&muet, "audio/flac").is_empty());
    assert!(cadences_declarees_pcm(&muet).is_empty());
}

#[test]
fn un_renderer_qui_declare_192_khz_recoit_le_flux_a_la_cadence_cible_8519() {
    let acceptees = cadences_acceptees(&[44_100, 48_000, 96_000, 192_000], None).unwrap();
    assert_eq!(
        acceptees,
        vec![96_000, 192_000],
        "seules les cadences déclarées"
    );
    let c = choix(true, actif());
    assert_eq!(negocier(&c, 48_000, &acceptees), Ok(192_000));
    assert_eq!(negocier(&c, 96_000, &acceptees), Ok(192_000));
    // 44,1 kHz : aucune cadence DÉCLARÉE de sa famille → refus nommé.
    assert_eq!(
        negocier(&c, 44_100, &acceptees),
        Err(RefusReseau::PlafondDuRenderer)
    );
}

#[test]
fn un_renderer_limite_a_48_khz_ramene_la_cible_8519() {
    // Ne déclare que 44,1 et 48 kHz : aucune cadence du suréchantillonneur
    // n'est sous son plafond.
    let acceptees = cadences_acceptees(&[44_100, 48_000], None).unwrap();
    assert!(acceptees.is_empty());
    assert_eq!(
        negocier(&choix(true, actif()), 44_100, &acceptees),
        Err(RefusReseau::PlafondDuRenderer)
    );
    // Déclare 96 kHz au plus, hors cadences candidates exactes pour 44,1 :
    // « la plus haute acceptée » → tout ce qui est sous 96 kHz.
    let acceptees = cadences_acceptees(&[96_000], Some(192_000)).unwrap();
    assert_eq!(acceptees, vec![96_000]);
    // Sans déclaration exacte, le plafond borne : 88,2 kHz pour du 44,1.
    let acceptees = cadences_acceptees(&[90_000], None).unwrap();
    assert_eq!(acceptees, vec![88_200]);
    assert_eq!(
        negocier(&choix(true, actif()), 44_100, &acceptees),
        Ok(88_200)
    );
}

#[test]
fn le_plafond_de_la_zone_borne_et_remplace_un_sink_muet_8519() {
    // Sink muet, plafond réglé à 192 kHz : la famille 44,1 monte à 176,4.
    let acceptees = cadences_acceptees(&[], Some(192_000)).unwrap();
    assert_eq!(acceptees, vec![88_200, 176_400, 96_000, 192_000]);
    assert_eq!(
        negocier(&choix(true, actif()), 44_100, &acceptees),
        Ok(176_400)
    );
    // Le plafond de la zone borne une déclaration plus haute.
    let acceptees = cadences_acceptees(&[384_000, 192_000], Some(96_000)).unwrap();
    assert_eq!(acceptees, vec![88_200, 96_000]);
}

#[test]
fn un_renderer_sans_protocol_info_exploitable_n_est_pas_surechantillonne_8519() {
    assert_eq!(
        cadences_acceptees(&[], None),
        Err(RefusReseau::CadencesDuRendererInconnues)
    );
    assert_eq!(
        RefusReseau::CadencesDuRendererInconnues.code(),
        "renderer_rates_unknown"
    );
}

#[test]
fn la_machine_et_les_plafonds_des_reglages_ont_leur_raison_8519() {
    let acceptees = cadences_acceptees(&[], Some(384_000)).unwrap();
    assert_eq!(
        negocier(&choix(false, actif()), 48_000, &acceptees),
        Err(RefusReseau::MachineTropLente)
    );
    let plafonne = UpsamplerSettings {
        max_rate_hz: Some(50_000),
        ..actif()
    };
    assert_eq!(
        negocier(&choix(true, plafonne), 48_000, &acceptees),
        Err(RefusReseau::PlafondsDesReglages)
    );
    let x2 = UpsamplerSettings {
        max_ratio: Some(2),
        ..actif()
    };
    assert_eq!(negocier(&choix(true, x2), 48_000, &acceptees), Ok(96_000));
}

#[test]
fn chaque_refus_se_relit_par_son_code_8519() {
    for r in [
        RefusReseau::SortieNonPriseEnCharge,
        RefusReseau::SourceNonPcm,
        RefusReseau::BitPerfectStrict,
        RefusReseau::FluxProgressif,
        RefusReseau::CadencesDuRendererInconnues,
        RefusReseau::PlafondDuRenderer,
        RefusReseau::MachineTropLente,
        RefusReseau::PlafondsDesReglages,
        RefusReseau::PisteTropLourde,
    ] {
        assert_eq!(RefusReseau::depuis_code(r.code()), Some(r));
    }
    assert!(sortie_prise_en_charge("dlna"));
    assert!(sortie_prise_en_charge("openhome"));
    for t in ["airplay", "chromecast", "local", "bluos"] {
        assert!(!sortie_prise_en_charge(t), "{t}");
    }
}

/// L'étage sur une piste entière : cadence ×4, longueur n·4 plus la queue du
/// noyau, aucune crête au-dessus de la pleine échelle, et le signal survit
/// (pas de silence).
#[test]
fn l_etage_porte_une_piste_entiere_a_la_cadence_cible_8519() {
    let trames = 44_100usize; // une seconde
    let mut pcm = Vec::with_capacity(trames * 4);
    for n in 0..trames {
        let x = 0.5 * (std::f64::consts::TAU * 1_000.0 * n as f64 / 44_100.0).sin();
        let mot = (x * 32_767.0).round() as i16;
        pcm.extend_from_slice(&mot.to_le_bytes());
        pcm.extend_from_slice(&mot.to_le_bytes());
    }
    let mut etage = EtageSurechantillonneur::nouveau(&actif(), 44_100, 176_400, 2).unwrap();
    let sortie = surechantillonner_pcm(&mut etage, &pcm, 16).unwrap();
    assert_eq!(sortie.len() % 8, 0, "trames 32 bits stéréo entières");
    let trames_sortie = sortie.len() / 8;
    assert!(
        trames_sortie >= trames * 4 && trames_sortie < trames * 4 + 176_400 / 10,
        "n·4 plus la queue du noyau : {trames_sortie}"
    );
    let mots: Vec<i32> = sortie
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let crete = mots.iter().map(|m| (*m as f64).abs()).fold(0.0, f64::max) / 2_147_483_648.0;
    assert!(crete > 0.3, "le signal survit : crête {crete}");
    assert!(crete <= 1.0, "aucune crête au-dessus de la pleine échelle");
    assert!(surechantillonner_pcm(&mut etage, &pcm, 20).is_err());
}

#[test]
fn la_reserve_de_la_cible_est_celle_du_greffon_8519() {
    let c = CibleReseau {
        de: 44_100,
        vers: 176_400,
        reglages: actif(),
    };
    let etage = EtageSurechantillonneur::nouveau(&actif(), 44_100, 176_400, 2).unwrap();
    assert!((c.reserve_db() - etage.reserve_db()).abs() < 1e-9);
    assert!(c.reserve_db() <= 0.0);
}
