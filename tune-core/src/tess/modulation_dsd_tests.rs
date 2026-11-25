//! Témoins du suréchantillonnage vers le DSD (rc14) : décision, capacités,
//! porteurs DoP et natif, stabilité et marge de niveau du modulateur T11b.

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
fn dop_jamais_refuse(_: u32, _: u16) -> bool {
    false
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
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400, 2),
        Err(MotifDsdRefuse::OutputNotLocal)
    );
    for mode in ["auto", "pcm", "", "inconnu"] {
        let f = faits(mode, &natif_absent);
        assert_eq!(
            decider_cadence(&f, TauxDsd::Dsd64, 2_822_400, 2),
            Err(MotifDsdRefuse::DsdModeNotEnabled),
            "{mode}"
        );
    }
}

#[test]
fn natif_si_le_pilote_le_declare_sinon_dop() {
    let f = faits("native", &natif_dsd64_seul);
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400, 2),
        Ok(TransportModulation::Native)
    );
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800, 2),
        Ok(TransportModulation::Dop)
    );
    // « dop » ne demande jamais le natif, même déclaré.
    let f = faits("dop", &natif_dsd64_seul);
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400, 2),
        Ok(TransportModulation::Dop)
    );
}

#[test]
fn plafond_de_zone_refus_dop_et_machine_lente() {
    let mut f = faits("dop", &natif_absent);
    f.plafond_zone_hz = Some(192_000);
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd64, 2_822_400, 2),
        Ok(TransportModulation::Dop)
    );
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800, 2),
        Err(MotifDsdRefuse::RateAboveZoneMax)
    );
    let refuse_352 = |c: u32, _: u16| c == 352_800;
    let mut f = faits("dop", &natif_absent);
    f.dop_refuse = &refuse_352;
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800, 2),
        Err(MotifDsdRefuse::DopRefusedByOutput)
    );
    let mut f = faits("dop", &natif_absent);
    f.temps_reel = Some([20.0, 3.0, 1.5]);
    assert!(decider_cadence(&f, TauxDsd::Dsd128, 5_644_800, 2).is_ok());
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd256, 11_289_600, 2),
        Err(MotifDsdRefuse::MachineTooSlow)
    );
    // Six voies : trois fois le coût de la stéréo.
    assert_eq!(
        decider_cadence(&f, TauxDsd::Dsd128, 5_644_800, 6),
        Err(MotifDsdRefuse::MachineTooSlow)
    );
    // Pas encore mesurée : rien n'est refusé pour ce motif.
    f.temps_reel = None;
    assert!(decider_cadence(&f, TauxDsd::Dsd256, 11_289_600, 2).is_ok());
}

#[test]
fn planifier_choisit_la_famille_de_la_source_et_refuse_l_impossible() {
    let f = faits("dop", &natif_absent);
    let p = planifier(&f, TauxDsd::Dsd64, 44_100, 2).unwrap();
    assert_eq!(
        (p.cadence_dsd, p.cadence_du_porteur()),
        (2_822_400, 176_400)
    );
    let p = planifier(&f, TauxDsd::Dsd64, 96_000, 2).unwrap();
    assert_eq!(
        (p.cadence_dsd, p.cadence_du_porteur()),
        (3_072_000, 192_000)
    );
    let p = planifier(&f, TauxDsd::Dsd256, 192_000, 2).unwrap();
    assert_eq!(p.cadence_du_porteur(), 768_000);
    // 705,6 kHz → DSD64 : rapport 4 < 8.
    assert_eq!(
        planifier(&f, TauxDsd::Dsd64, 705_600, 2),
        Err(MotifDsdRefuse::SourceRateNotModulable)
    );
    assert_eq!(
        planifier(&f, TauxDsd::Dsd64, 32_000, 2),
        Err(MotifDsdRefuse::SourceRateNotModulable)
    );
    assert_eq!(
        planifier(&f, TauxDsd::Dsd64, 44_100, 9),
        Err(MotifDsdRefuse::ChannelsNotSupported)
    );
}

// ── Capacités publiées ──────────────────────────────────────────────

#[test]
fn les_capacites_publient_chaque_cible_avec_son_transport_ou_son_motif() {
    let mut f = faits("native", &natif_dsd64_seul);
    f.plafond_zone_hz = Some(384_000);
    let c = capacites(&f, true);
    assert_eq!(c.modulator_order, 7);
    assert!(c.output_local && c.volume_protection);
    let d64 = c.cible(TauxDsd::Dsd64).unwrap();
    assert!(d64.famille_44k1.accepted);
    assert_eq!(
        d64.famille_44k1.transport,
        Some(TransportModulation::Native)
    );
    assert_eq!(d64.famille_44k1.dop_rate_hz, None);
    // Famille 48 : pilote sans 3,072 MHz → DoP à 192 kHz, motif natif dit.
    assert_eq!(d64.family_48k.transport, Some(TransportModulation::Dop));
    assert_eq!(d64.family_48k.dop_rate_hz, Some(192_000));
    assert_eq!(
        d64.family_48k.native_reason.as_deref(),
        Some("cadence_non_declaree_par_le_pilote")
    );
    let d128 = c.cible(TauxDsd::Dsd128).unwrap();
    assert_eq!(d128.famille_44k1.dop_rate_hz, Some(352_800));
    let d256 = c.cible(TauxDsd::Dsd256).unwrap();
    assert!(!d256.famille_44k1.accepted);
    assert_eq!(
        d256.famille_44k1.reason,
        Some(MotifDsdRefuse::RateAboveZoneMax)
    );
    assert_eq!(d256.famille_44k1.transport, None);

    // Le contrat JSON : `accepted`, `transport`, `reason` à plat, et la
    // famille 48 kHz à part.
    let v = serde_json::to_value(&c).unwrap();
    let t = &v["targets"][2];
    assert_eq!(t["rate"], "dsd256");
    assert_eq!(t["accepted"], false);
    assert_eq!(t["reason"], "rate_above_zone_max");
    assert_eq!(t["rate_hz"], 11_289_600);
    assert_eq!(t["family_48k"]["rate_hz"], 12_288_000);
    assert_eq!(v["targets"][0]["transport"], "native");
}

#[test]
fn une_zone_auto_ne_publie_aucune_cible_acceptee() {
    let c = capacites(&faits("auto", &natif_absent), true);
    for t in &c.targets {
        assert!(!t.famille_44k1.accepted && !t.family_48k.accepted);
        assert_eq!(
            t.famille_44k1.reason,
            Some(MotifDsdRefuse::DsdModeNotEnabled)
        );
    }
}

// ── Porteurs ────────────────────────────────────────────────────────

fn sinus(fs: u32, f0: f64, amplitude: f64, trames: usize, canaux: usize) -> Vec<f64> {
    (0..trames)
        .flat_map(|i| {
            let v = amplitude * (2.0 * std::f64::consts::PI * f0 * i as f64 / f64::from(fs)).sin();
            std::iter::repeat_n(v, canaux)
        })
        .collect()
}

fn plan(transport: TransportModulation, taux: TauxDsd, fs: u32, voies: u16) -> PlanModulation {
    let f = faits(
        if transport == TransportModulation::Native {
            "native"
        } else {
            "dop"
        },
        &|_, _| Ok(()),
    );
    let p = planifier(&f, taux, fs, voies).unwrap();
    assert_eq!(p.transport, transport);
    p
}

/// Le train DSD (octets MSB-first entrelacés) porté par un flux DoP 24 bits.
fn dsd_du_dop(dop: &[u8], canaux: usize) -> Vec<u8> {
    let mut dsd = Vec::new();
    for trame in dop.chunks_exact(3 * canaux) {
        let mut hauts = Vec::new();
        let mut bas = Vec::new();
        for c in 0..canaux {
            hauts.push(trame[3 * c + 1]);
            bas.push(trame[3 * c]);
        }
        dsd.extend(hauts);
        dsd.extend(bas);
    }
    dsd
}

#[test]
fn dop_marqueurs_alternes_debit_et_train_identique_au_natif() {
    for (taux, rapport) in [
        (TauxDsd::Dsd64, 64usize),
        (TauxDsd::Dsd128, 128),
        (TauxDsd::Dsd256, 256),
    ] {
        let trames = 4_410;
        let pcm = sinus(44_100, 997.0, 0.5, trames, 2);
        let mut dop = ProducteurDsd::new(plan(TransportModulation::Dop, taux, 44_100, 2)).unwrap();
        let mut natif =
            ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100, 2)).unwrap();
        let entete = dop.entete(Some(1_000));
        assert_eq!(&entete[..4], b"RIFF");
        let cadence = u32::from_le_bytes(entete[24..28].try_into().unwrap());
        let bits = u16::from_le_bytes(entete[34..36].try_into().unwrap());
        assert_eq!(
            (cadence, bits),
            (44_100 * rapport as u32 / 16, 24),
            "{taux:?}"
        );

        let (mut a, mut b) = (Vec::new(), Vec::new());
        // Découpage hostile : 1, 7, puis le reste.
        for morceau in [&pcm[..2], &pcm[2..16], &pcm[16..]] {
            dop.moduler(morceau, &mut a).unwrap();
            natif.moduler(morceau, &mut b).unwrap();
        }
        // Débit : rapport / 16 trames DoP par trame PCM, 3 octets par voie.
        assert_eq!(a.len(), trames * rapport / 16 * 3 * 2, "{taux:?}");
        // Natif : rapport / 8 octets par voie et par trame PCM.
        assert_eq!(b.len(), trames * rapport / 8 * 2, "{taux:?}");
        // Marqueurs : 0x05 d'abord, alternés, identiques sur les deux voies.
        let mut attendu = 0x05u8;
        for trame in a.as_chunks::<6>().0 {
            assert_eq!(trame[2], attendu);
            assert_eq!(trame[5], attendu);
            attendu = if attendu == 0x05 { 0xFA } else { 0x05 };
        }
        // Le DoP porte EXACTEMENT le train du natif.
        assert_eq!(dsd_du_dop(&a, 2), b, "{taux:?}");
        #[cfg(feature = "local-audio")]
        assert!(crate::outputs::local::is_dop_pcm(&a, 24, 2));

        // Clôture : nombre pair de trames DoP.
        let mut fin = Vec::new();
        dop.terminer(&mut fin).unwrap();
        assert_eq!((a.len() + fin.len()) / 6 % 2, 0);
        assert_eq!(fin.last(), Some(&0xFA));
    }
}

#[test]
fn natif_en_tete_du_flux_dsd_brut() {
    let p = ProducteurDsd::new(plan(
        TransportModulation::Native,
        TauxDsd::Dsd128,
        96_000,
        2,
    ))
    .unwrap();
    let e = crate::audio::dsd_brut::lire_entete(&p.entete(None)).unwrap();
    assert_eq!((e.cadence, e.canaux), (6_144_000, 2));
}

// ── Stabilité et marge de niveau (T11b) ─────────────────────────────

/// Moyenne des bits (±1) d'une voie : la modulation portée.
fn densite(octets: &[u8], canaux: usize, canal: usize) -> f64 {
    let mut s = 0i64;
    let mut n = 0i64;
    for o in octets.iter().skip(canal).step_by(canaux) {
        s += 2 * i64::from(o.count_ones()) - 8;
        n += 8;
    }
    s as f64 / n as f64
}

#[test]
fn stable_a_0_dbfs_et_au_dela_a_chaque_cadence() {
    for taux in [TauxDsd::Dsd64, TauxDsd::Dsd128, TauxDsd::Dsd256] {
        let trames = 22_050;
        let signaux: Vec<(&str, Vec<f64>)> = vec![
            ("1 kHz 0 dBFS", sinus(44_100, 1_000.0, 1.0, trames, 2)),
            ("20 kHz 0 dBFS", sinus(44_100, 20_000.0, 1.0, trames, 2)),
            (
                "carré pleine échelle",
                (0..trames)
                    .flat_map(|i| {
                        let v = if (i / 50) % 2 == 0 { 1.0 } else { -1.0 };
                        [v, v]
                    })
                    .collect(),
            ),
            (
                "intersample +3 dB",
                (0..trames)
                    .flat_map(|i| {
                        let v = std::f64::consts::SQRT_2
                            * (std::f64::consts::PI * i as f64 / 2.0 + std::f64::consts::PI / 4.0)
                                .sin()
                                .clamp(-1.0, 1.0);
                        [v, v]
                    })
                    .collect(),
            ),
        ];
        for (nom, pcm) in signaux {
            let mut p =
                ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100, 2)).unwrap();
            let mut sortie = Vec::new();
            p.moduler(&pcm, &mut sortie).unwrap();
            assert_eq!(p.reinitialisations(), 0, "{taux:?} {nom} : instabilité");
            // T11b : entrée du quantificateur ≤ 1,48 au pire (DSD256, crête
            // intersample tenue au limiteur), loin du seuil de 4.
            assert!(
                p.entree_max() < 1.6,
                "{taux:?} {nom} : entrée {}",
                p.entree_max()
            );
        }
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
                ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100, 2)).unwrap();
            let pcm = vec![niveau; 2 * 8_820];
            let mut sortie = Vec::new();
            p.moduler(&pcm, &mut sortie).unwrap();
            // Le régime établi : la seconde moitié (le filtre a convergé).
            let moitie = &sortie[sortie.len() / 2..];
            let d = densite(moitie, 2, 0);
            assert!(
                (d - attendu).abs() < 0.01,
                "{taux:?} entrée {niveau} : densité {d}, attendu {attendu}"
            );
            assert_eq!(p.reinitialisations(), 0, "{taux:?} {niveau}");
        }
    }
}

// ── Registre ────────────────────────────────────────────────────────

#[test]
fn le_registre_suit_la_piste_et_garde_la_suivante() {
    let zone = 9_430_001;
    let m = ModulationEnService {
        rate: TauxDsd::Dsd64,
        rate_hz: 2_822_400,
        transport: TransportModulation::Dop,
        order: 7,
        carrier_rate_hz: 176_400,
        source_rate_hz: 44_100,
        channels: 2,
        plugins_in_stream: false,
    };
    noter(zone, Some(1), EtatModulation::EnService(m.clone()));
    noter(
        zone,
        Some(2),
        EtatModulation::Repli(MotifDsdRefuse::SourceRateNotModulable),
    );
    assert_eq!(etat_pour(zone, Some(1)), Some(EtatModulation::EnService(m)));
    assert_eq!(
        etat_pour(zone, Some(2)),
        Some(EtatModulation::Repli(
            MotifDsdRefuse::SourceRateNotModulable
        ))
    );
    noter(
        zone,
        Some(3),
        EtatModulation::Repli(MotifDsdRefuse::MachineTooSlow),
    );
    assert_eq!(etat_pour(zone, Some(1)), None, "deux pistes au plus");
    oublier(zone, Some(3));
    assert_eq!(etat_pour(zone, Some(3)), None);
}
