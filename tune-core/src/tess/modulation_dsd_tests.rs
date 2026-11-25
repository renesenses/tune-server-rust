//! Témoins du suréchantillonnage vers le DSD (rc14) : décision, capacités,
//! porteurs DoP et natif, voies, enchaînement, volume et headroom avant le
//! modulateur, stabilité et marge de niveau du modulateur T11b.

use super::*;

fn natif_absent(_: u32, _: u16) -> Result<(), &'static str> {
    Err("sortie_non_asio")
}
fn natif_dsd64_seul(c: u32, _: u16) -> Result<(), &'static str> {
    if c == 2_822_400 {
        Ok(())
    } else {
        Err("cadence_non_declaree_par_le_pilote")
    }
}
fn dop_jamais_refuse(_: u32, _: u16) -> Option<MotifDsdRefuse> {
    None
}

fn faits<'a>(
    dsd_mode: &'a str,
    natif: &'a dyn Fn(u32, u16) -> Result<(), &'static str>,
) -> FaitsSortieDsd<'a> {
    FaitsSortieDsd {
        sortie_locale: true,
        dsd_mode,
        plafond_zone_hz: None,
        natif,
        dop_refuse: &dop_jamais_refuse,
        temps_reel: Some([20.0, 10.0, 5.0]),
    }
}

// ── Décision ────────────────────────────────────────────────────────

#[test]
fn une_sortie_reseau_ou_une_zone_sans_dsd_declare_est_refusee() {
    let mut f = faits("dop", &natif_absent);
    f.sortie_locale = false;
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400),
        Err(MotifDsdRefuse::OutputNotLocal)
    );
    for mode in ["auto", "pcm", "", "inconnu"] {
        let f = faits(mode, &natif_absent);
        assert_eq!(
            decider_cadence(&f, TauxDsd::Dsd64, 2_822_400),
            Err(MotifDsdRefuse::DsdModeNotEnabled),
            "{mode}"
        );
    }
}

#[test]
fn natif_si_le_pilote_le_declare_sinon_dop() {
    let f = faits("native", &natif_dsd64_seul);
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400),
        Ok(TransportModulation::Native)
    );
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800),
        Ok(TransportModulation::Dop)
    );
    let f = faits("dop", &natif_dsd64_seul);
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400),
        Ok(TransportModulation::Dop)
    );
}

#[test]
fn plafond_de_zone_porteur_refuse_ou_rompu_et_machine_lente() {
    let mut f = faits("dop", &natif_absent);
    f.plafond_zone_hz = Some(192_000);
    assert!(decider_cadence(&f, TauxDsd::Dsd64, 2_822_400).is_ok());
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800),
        Err(MotifDsdRefuse::RateAboveZoneMax)
    );
    let rompu_352 =
        |c: u32, _: u16| (c == 352_800).then_some(MotifDsdRefuse::CarrierRateNotSupported);
    let mut f = faits("dop", &natif_absent);
    f.dop_refuse = &rompu_352;
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800),
        Err(MotifDsdRefuse::CarrierRateNotSupported)
    );
    assert!(decider_cadence(&f, TauxDsd::Dsd64, 2_822_400).is_ok());
    let mut f = faits("dop", &natif_absent);
    f.temps_reel = Some([20.0, 3.0, 1.5]);
    assert!(decider_cadence(&f, TauxDsd::Dsd128, 5_644_800).is_ok());
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd256, 11_289_600),
        Err(MotifDsdRefuse::MachineTooSlow)
    );
    // Pas encore mesurée : rien n'est refusé pour ce motif.
    f.temps_reel = None;
    assert!(decider_cadence(&f, TauxDsd::Dsd256, 11_289_600).is_ok());
}

#[test]
fn planifier_porte_le_mono_en_stereo_et_refuse_le_multicanal() {
    let f = faits("dop", &natif_absent);
    let p = planifier(&f, TauxDsd::Dsd64, 44_100, 2).unwrap();
    assert_eq!(
        (p.cadence_dsd, p.cadence_du_porteur()),
        (2_822_400, 176_400)
    );
    // Mono : porteur STÉRÉO (un DoP mono serait coupé par la sortie).
    let p = planifier(&f, TauxDsd::Dsd64, 44_100, 1).unwrap();
    assert_eq!((p.voies_source, p.voies), (1, 2));
    // 5.1 et voies inconnues : repli en PCM, motif dit.
    for v in [0, 3, 6, 8] {
        assert_eq!(
            planifier(&f, TauxDsd::Dsd64, 44_100, v),
            Err(MotifDsdRefuse::ChannelsNotSupported),
            "{v} voies"
        );
    }
    let p = planifier(&f, TauxDsd::Dsd64, 96_000, 2).unwrap();
    assert_eq!(
        (p.cadence_dsd, p.cadence_du_porteur()),
        (3_072_000, 192_000)
    );
    let p = planifier(&f, TauxDsd::Dsd256, 192_000, 2).unwrap();
    assert_eq!(p.cadence_du_porteur(), 768_000);
    assert_eq!(
        planifier(&f, TauxDsd::Dsd64, 705_600, 2),
        Err(MotifDsdRefuse::SourceRateNotModulable)
    );
    assert_eq!(
        planifier(&f, TauxDsd::Dsd64, 32_000, 2),
        Err(MotifDsdRefuse::SourceRateNotModulable)
    );
}

// ── Capacités publiées ──────────────────────────────────────────────

#[test]
fn les_capacites_publient_chaque_cible_avec_son_transport_ou_son_motif() {
    let mut f = faits("native", &natif_dsd64_seul);
    f.plafond_zone_hz = Some(384_000);
    let c = capacites(&f);
    assert_eq!(c.modulator_order, 7);
    assert!(c.output_local && c.machine_measured);
    assert_eq!(c.volume_placement, "before_dsd_modulator");
    let d64 = c.cible(TauxDsd::Dsd64).unwrap();
    assert_eq!(
        d64.famille_44k1.transport,
        Some(TransportModulation::Native)
    );
    assert_eq!(d64.family_48k.transport, Some(TransportModulation::Dop));
    assert_eq!(d64.family_48k.dop_rate_hz, Some(192_000));
    assert_eq!(
        d64.family_48k.native_reason.as_deref(),
        Some("cadence_non_declaree_par_le_pilote")
    );
    let d256 = c.cible(TauxDsd::Dsd256).unwrap();
    assert!(!d256.famille_44k1.accepted);
    assert_eq!(
        d256.famille_44k1.reason,
        Some(MotifDsdRefuse::RateAboveZoneMax)
    );
    let v = serde_json::to_value(&c).unwrap();
    let t = &v["targets"][2];
    assert_eq!(t["rate"], "dsd256");
    assert_eq!(t["reason"], "rate_above_zone_max");
    assert_eq!(t["family_48k"]["rate_hz"], 12_288_000);
    assert_eq!(v["targets"][0]["transport"], "native");
}

#[test]
fn une_zone_auto_ne_publie_aucune_cible_acceptee() {
    let c = capacites(&faits("auto", &natif_absent));
    for t in &c.targets {
        assert!(!t.famille_44k1.accepted && !t.family_48k.accepted);
        assert_eq!(
            t.famille_44k1.reason,
            Some(MotifDsdRefuse::DsdModeNotEnabled)
        );
    }
}

// ── Porteurs ────────────────────────────────────────────────────────

fn sinus(fs: u32, f0: f64, amplitude: f64, trames: usize) -> Vec<f64> {
    (0..trames)
        .flat_map(|i| {
            let v = amplitude * (2.0 * std::f64::consts::PI * f0 * i as f64 / f64::from(fs)).sin();
            [v, v]
        })
        .collect()
}

fn plan(transport: TransportModulation, taux: TauxDsd, fs: u32) -> PlanModulation {
    let f = faits(
        if transport == TransportModulation::Native {
            "native"
        } else {
            "dop"
        },
        &|_, _| Ok(()),
    );
    let p = planifier(&f, taux, fs, 2).unwrap();
    assert_eq!(p.transport, transport);
    p
}

/// Le train DSD (octets MSB-first entrelacés) porté par un flux DoP 24 bits.
fn dsd_du_dop(dop: &[u8]) -> Vec<u8> {
    let mut dsd = Vec::new();
    for t in dop.as_chunks::<6>().0 {
        dsd.extend([t[1], t[4], t[0], t[3]]);
    }
    dsd
}

fn marqueurs_alternes_depuis_05(dop: &[u8]) {
    let mut attendu = 0x05u8;
    for t in dop.as_chunks::<6>().0 {
        assert_eq!((t[2], t[5]), (attendu, attendu));
        attendu = if attendu == 0x05 { 0xFA } else { 0x05 };
    }
}

#[test]
fn dop_marqueurs_alternes_debit_pair_et_train_identique_au_natif() {
    for (taux, rapport) in [
        (TauxDsd::Dsd64, 64usize),
        (TauxDsd::Dsd128, 128),
        (TauxDsd::Dsd256, 256),
    ] {
        let trames = 4_410;
        let pcm = sinus(44_100, 997.0, 0.5, trames);
        let mut dop = ProducteurDsd::new(plan(TransportModulation::Dop, taux, 44_100)).unwrap();
        let mut natif =
            ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
        let entete = dop.entete(Some(1_000));
        assert_eq!(&entete[..4], b"RIFF");
        let cadence = u32::from_le_bytes(entete[24..28].try_into().unwrap());
        let voies = u16::from_le_bytes(entete[22..24].try_into().unwrap());
        assert_eq!(
            (cadence, voies),
            (44_100 * rapport as u32 / 16, 2),
            "{taux:?}"
        );
        let (mut a, mut b) = (Vec::new(), Vec::new());
        // Découpage hostile : 1 trame, 7 trames, puis le reste.
        for morceau in [&pcm[..2], &pcm[2..16], &pcm[16..]] {
            let avant = a.len();
            dop.moduler(morceau, &mut a).unwrap();
            // Chaque appel émet un nombre PAIR de trames DoP.
            assert_eq!((a.len() - avant) / 6 % 2, 0);
            natif.moduler(morceau, &mut b).unwrap();
        }
        assert_eq!(a.len(), trames * rapport / 16 * 3 * 2, "{taux:?}");
        assert_eq!(b.len(), trames * rapport / 8 * 2, "{taux:?}");
        marqueurs_alternes_depuis_05(&a);
        assert_eq!(dsd_du_dop(&a), b, "{taux:?}");
        #[cfg(feature = "local-audio")]
        assert!(crate::outputs::local::is_dop_pcm(&a, 24, 2));
    }
}

#[test]
fn natif_en_tete_du_flux_dsd_brut() {
    let p = ProducteurDsd::new(plan(TransportModulation::Native, TauxDsd::Dsd128, 96_000)).unwrap();
    let e = crate::audio::dsd_brut::lire_entete(&p.entete(None)).unwrap();
    assert_eq!((e.cadence, e.canaux), (6_144_000, 2));
}

// ── Enchaînement ────────────────────────────────────────────────────

/// Deux pistes enchaînées (même famille, même cadence) : l'état passe de
/// l'une à l'autre. Le flux concaténé est EXACTEMENT celui d'une seule
/// modulation des deux pistes bout à bout — ni silence, ni clôture, ni
/// marche — et la seconde piste commence par 0x05.
#[test]
fn enchainement_sans_rupture_ni_silence() {
    // Longueurs quelconques (3 001 et 2 003 trames) : le filtre du
    // suréchantillonneur porte la fin de la piste 1 dans la piste 2.
    let p1 = sinus(44_100, 997.0, 0.5, 3_001);
    let p2 = sinus(44_100, 1_499.0, 0.5, 2_003);
    for transport in [TransportModulation::Dop, TransportModulation::Native] {
        let pl = plan(transport, TauxDsd::Dsd64, 44_100);
        let mut seul = ProducteurDsd::new(pl).unwrap();
        let mut tout = Vec::new();
        seul.moduler(&[p1.clone(), p2.clone()].concat(), &mut tout)
            .unwrap();

        let zone = 9_430_101 + transport as i64;
        let g1 = ouvrir_une_generation(zone);
        let mut a = ProducteurDsd::new(pl).unwrap();
        let mut piste1 = Vec::new();
        a.moduler(&p1, &mut piste1).unwrap();
        deposer_pour_la_suite(zone, g1, a);
        let g2 = ouvrir_une_generation(zone);
        let mut b = reprendre_la_suite(zone, g2, pl).expect("l'état continue");
        let mut piste2 = Vec::new();
        b.moduler(&p2, &mut piste2).unwrap();

        let enchaine = [piste1.clone(), piste2.clone()].concat();
        if transport == TransportModulation::Dop {
            marqueurs_alternes_depuis_05(&piste1);
            marqueurs_alternes_depuis_05(&piste2);
            assert_eq!(piste1.len() / 6 % 2, 0);
        }
        assert_eq!(enchaine, tout[..enchaine.len()], "{transport:?}");
        assert!(
            tout.len() - enchaine.len() <= 12,
            "au plus une paire en attente"
        );
    }
}

#[test]
fn pas_d_enchainement_sur_un_plan_different_ou_apres_une_autre_piste() {
    let zone = 9_430_110;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    let g = ouvrir_une_generation(zone);
    deposer_pour_la_suite(zone, g, ProducteurDsd::new(pl).unwrap());
    let g2 = ouvrir_une_generation(zone);
    let autre = plan(TransportModulation::Dop, TauxDsd::Dsd128, 44_100);
    assert!(reprendre_la_suite(zone, g2, autre).is_none());
    // Une piste intercalée (génération sautée) : rien ne continue.
    let g3 = ouvrir_une_generation(zone);
    deposer_pour_la_suite(zone, g3 - 2, ProducteurDsd::new(pl).unwrap());
    assert!(reprendre_la_suite(zone, g3, pl).is_none());
}

// ── Volume, headroom, stabilité, marge (T11b) ───────────────────────

/// Moyenne des bits (±1) d'une voie : la modulation portée.
fn densite(octets: &[u8], canal: usize) -> f64 {
    let mut s = 0i64;
    let mut n = 0i64;
    for o in octets.iter().skip(canal).step_by(2) {
        s += 2 * i64::from(o.count_ones()) - 8;
        n += 8;
    }
    s as f64 / n as f64
}

#[test]
fn le_volume_et_le_gain_fixe_s_appliquent_avant_le_modulateur() {
    for (volume, fixe, attendu) in [
        (1.0, 1.0, 0.5),
        (0.5, 1.0, 0.25),
        (1.0, 0.5, 0.25),
        (0.5, 0.5, 0.125),
        (0.0, 1.0, 0.0),
    ] {
        let mut p =
            ProducteurDsd::new(plan(TransportModulation::Native, TauxDsd::Dsd64, 44_100)).unwrap();
        p.definir_volume(volume);
        p.definir_gain_fixe(fixe);
        let mut sortie = Vec::new();
        p.moduler(&vec![1.0; 2 * 8_820], &mut sortie).unwrap();
        let d = densite(&sortie[sortie.len() / 2..], 0);
        assert!(
            (d - attendu).abs() < 0.01,
            "volume {volume} × {fixe} : {d}, attendu {attendu}"
        );
    }
}

#[test]
fn stable_a_0_dbfs_et_au_dela_a_chaque_cadence() {
    for taux in [TauxDsd::Dsd64, TauxDsd::Dsd128, TauxDsd::Dsd256] {
        let trames = 22_050;
        let carre: Vec<f64> = (0..trames)
            .flat_map(|i| {
                let v = if (i / 50) % 2 == 0 { 1.0 } else { -1.0 };
                [v, v]
            })
            .collect();
        for (nom, pcm) in [
            ("1 kHz 0 dBFS", sinus(44_100, 1_000.0, 1.0, trames)),
            ("20 kHz 0 dBFS", sinus(44_100, 20_000.0, 1.0, trames)),
            ("carré pleine échelle", carre),
            ("intersample +3 dB", intersample(trames)),
        ] {
            let mut p =
                ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
            let mut sortie = Vec::new();
            p.moduler(&pcm, &mut sortie).unwrap();
            assert_eq!(p.reinitialisations(), 0, "{taux:?} {nom} : instabilité");
            assert!(p.entree_max() < 1.6, "{taux:?} {nom} : {}", p.entree_max());
        }
    }
}

/// fs/4 à ±1,0 exactement sur les échantillons : la crête reconstruite vaut
/// +3 dB (√2).
pub(super) fn intersample(trames: usize) -> Vec<f64> {
    (0..trames)
        .flat_map(|i| {
            let v = (std::f64::consts::SQRT_2
                * (std::f64::consts::PI * i as f64 / 2.0 + std::f64::consts::PI / 4.0).sin())
            .clamp(-1.0, 1.0);
            [v, v]
        })
        .collect()
}

/// Q9 et le headroom de TESS : un master chaud (crête intersample à +3 dB)
/// atteint le limiteur à ±60 % SANS headroom ; avec les −3 dB automatiques
/// appliqués avant le modulateur, jamais.
#[test]
fn le_headroom_garde_les_cretes_intersample_sous_le_limiteur() {
    for taux in [TauxDsd::Dsd64, TauxDsd::Dsd256] {
        let pcm = intersample(22_050);
        let mut sans = ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
        let mut sortie = Vec::new();
        sans.moduler(&pcm, &mut sortie).unwrap();
        assert!(sans.ecretages() > 0, "{taux:?} : contre-épreuve");
        let mut avec = ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
        avec.definir_gain_fixe(10f64.powf(crate::tess::chaine::HEADROOM_AUTO_DB / 20.0));
        sortie.clear();
        avec.moduler(&pcm, &mut sortie).unwrap();
        assert_eq!(avec.ecretages(), 0, "{taux:?}");
        assert_eq!(avec.reinitialisations(), 0, "{taux:?}");
    }
}

/// La marge de T11b : 0 dBFS PCM = 50 % de modulation, plafond à 60 %
/// (+1,6 dB). Le producteur ne voit jamais plus que ±1,0 (le PCM y arrive en
/// entiers 32 bits) ; 1,5 éprouve le limiteur au-delà de ce qui peut arriver.
#[test]
fn marge_de_niveau_0_dbfs_a_50_pour_cent_jamais_au_dela_de_60() {
    for taux in [TauxDsd::Dsd64, TauxDsd::Dsd128, TauxDsd::Dsd256] {
        for (niveau, attendu) in [
            (1.0, 0.5),
            (-1.0, -0.5),
            (0.5, 0.25),
            (1.5, 0.6),
            (-1.5, -0.6),
        ] {
            let mut p =
                ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
            let mut sortie = Vec::new();
            p.moduler(&vec![niveau; 2 * 8_820], &mut sortie).unwrap();
            let d = densite(&sortie[sortie.len() / 2..], 0);
            assert!((d - attendu).abs() < 0.01, "{taux:?} {niveau} : {d}");
            assert_eq!(p.reinitialisations(), 0, "{taux:?} {niveau}");
        }
    }
}

// ── Registres ───────────────────────────────────────────────────────

fn en_service() -> ModulationEnService {
    ModulationEnService {
        rate: TauxDsd::Dsd64,
        rate_hz: 2_822_400,
        transport: TransportModulation::Dop,
        order: 7,
        carrier_rate_hz: 176_400,
        source_rate_hz: 44_100,
        source_channels: 2,
        channels: 2,
        plugins_in_stream: false,
        headroom_db: -3.0,
        continued: false,
    }
}

/// « Répéter la piste » : la même piste, deux flux. L'état suit la SESSION.
#[test]
fn le_registre_suit_le_flux_pas_la_piste() {
    let zone = 9_430_120;
    let cle = |s: Option<&str>| CleFlux {
        track_id: Some(1),
        stream_id: s.map(str::to_owned),
    };
    noter(
        zone,
        cle(Some("s1")),
        EtatModulation::EnService(en_service()),
    );
    noter(
        zone,
        cle(None),
        EtatModulation::Repli(MotifDsdRefuse::SourceRateNotModulable),
    );
    assert!(matches!(
        etat_pour(zone, Some("s1"), Some(1)),
        Some(EtatModulation::EnService(_))
    ));
    // La même piste rejouée par une autre session (repli) : le repli.
    assert_eq!(
        etat_pour(zone, Some("s2"), Some(1)),
        Some(EtatModulation::Repli(
            MotifDsdRefuse::SourceRateNotModulable
        ))
    );
    oublier(zone, Some(1));
    assert_eq!(etat_pour(zone, Some("s2"), Some(1)), None);
    assert!(etat_pour(zone, Some("s1"), Some(1)).is_some());
}

#[test]
fn une_rupture_constatee_est_retenue() {
    let nom = "DAC de test rupture rc14";
    assert!(!rupture_connue(nom, 352_800, 2));
    noter_rupture(nom, 352_800, 2);
    assert!(rupture_connue(nom, 352_800, 2));
    assert!(!rupture_connue(nom, 176_400, 2));
    assert!(rupture_recente(nom, Duration::from_secs(5)));
}
