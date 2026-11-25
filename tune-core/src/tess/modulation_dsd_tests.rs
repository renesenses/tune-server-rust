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
    let c = capacites(&f, true);
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

// ── Enchaînement ───────────────────────────────────────────────────

/// L'ordre RÉEL du pré-armement gapless (relecture v2) : la piste 2 est
/// résolue (lien annoncé) pendant que le producteur de la piste 1 tourne
/// encore ; elle n'attend l'état qu'au moment de moduler son premier bloc,
/// et le reçoit dès que la piste 1 le dépose. Le flux enchaîné est alors
/// EXACTEMENT celui d'une seule modulation des deux pistes bout à bout.
#[test]
fn enchainement_dans_l_ordre_reel_du_pre_armement() {
    let p1 = sinus(44_100, 997.0, 0.5, 3_001);
    let p2 = sinus(44_100, 1_499.0, 0.5, 2_003);
    for (n, transport) in [TransportModulation::Dop, TransportModulation::Native]
        .into_iter()
        .enumerate()
    {
        let zone = 9_430_101 + n as i64;
        let pl = plan(transport, TauxDsd::Dsd64, 44_100);
        let mut seul = ProducteurDsd::new(pl).unwrap();
        let mut tout = Vec::new();
        seul.moduler(&[p1.clone(), p2.clone()].concat(), &mut tout)
            .unwrap();

        // Piste 1 : son producteur démarre.
        let s1 = format!("session-1-{n}");
        ouvrir_le_relais(zone, &s1, pl);
        // Pré-armement de la piste 2 (piste 1 encore en production) : lien.
        annoncer_la_suivante(zone, 2, &s1);
        let lien = prendre_le_lien(zone, Some(2), pl).expect("lien explicite");
        // Le producteur de la piste 2 attend, avant son premier bloc.
        let attente = std::thread::spawn(move || {
            attendre_le_relais(&lien, pl, Duration::from_secs(10), &|| false)
        });
        // La piste 1 finit sa production, puis dépose son état.
        let mut a = ProducteurDsd::new(pl).unwrap();
        let mut piste1 = Vec::new();
        a.moduler(&p1, &mut piste1).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        deposer_au_relais(&s1, a);
        let mut b = attente
            .join()
            .unwrap()
            .expect("la piste 2 reprend l'état de la piste 1");
        let mut piste2 = Vec::new();
        b.moduler(&p2, &mut piste2).unwrap();
        let enchaine = [piste1.clone(), piste2.clone()].concat();
        if transport == TransportModulation::Dop {
            marqueurs_alternes_depuis_05(&piste1);
            marqueurs_alternes_depuis_05(&piste2);
        }
        assert_eq!(enchaine, tout[..enchaine.len()], "{transport:?}");
        assert!(tout.len() - enchaine.len() <= 12);
    }
}

/// Sans lien explicite, rien ne continue : une piste sans rapport (pas
/// annoncée comme suivante), une autre piste que celle annoncée, un plan
/// incompatible, une piste précédente abandonnée (saut, arrêt).
#[test]
fn aucun_etat_n_est_repris_sans_lien_explicite() {
    let zone = 9_430_110;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "s-a", pl);
    deposer_au_relais("s-a", ProducteurDsd::new(pl).unwrap());
    // Aucun lien annoncé : même un état tout frais ne passe pas.
    assert!(prendre_le_lien(zone, Some(5), pl).is_none());
    // Annoncé pour la piste 5 ; c'est la piste 6 qui se résout.
    annoncer_la_suivante(zone, 5, "s-a");
    assert!(prendre_le_lien(zone, Some(6), pl).is_none());
    // Plan incompatible.
    annoncer_la_suivante(zone, 5, "s-a");
    let autre = plan(TransportModulation::Dop, TauxDsd::Dsd128, 44_100);
    assert!(prendre_le_lien(zone, Some(5), autre).is_none());
    // Précédente abandonnée pendant l'attente : la suivante part à neuf.
    ouvrir_le_relais(zone, "s-b", pl);
    annoncer_la_suivante(zone, 7, "s-b");
    let lien = prendre_le_lien(zone, Some(7), pl).unwrap();
    let attente = std::thread::spawn(move || {
        attendre_le_relais(&lien, pl, Duration::from_secs(10), &|| false)
    });
    std::thread::sleep(Duration::from_millis(50));
    abandonner_le_relais("s-b");
    assert!(attente.join().unwrap().is_none());
    // La suivante elle-même annulée (sa session fermée) : elle n'attend pas.
    ouvrir_le_relais(zone, "s-c", pl);
    assert!(attendre_le_relais("s-c", pl, Duration::from_secs(10), &|| true).is_none());
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
        p.fixer_volume(volume);
        p.definir_gain_fixe(fixe);
        let mut sortie = Vec::new();
        p.moduler(&vec![1.0; 2 * 8_820], &mut sortie).unwrap();
        // TOUT le signal, dès le premier échantillon (pas seulement le régime
        // établi) : le niveau est posé avant le premier bloc.
        let d = densite(&sortie, 0);
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
/// atteint le limiteur à ±60 % SANS marge ; avec la marge propre du
/// modulateur (−3 dB) appliquée avant lui, jamais.
#[test]
fn le_headroom_garde_les_cretes_intersample_sous_le_limiteur() {
    for taux in [TauxDsd::Dsd64, TauxDsd::Dsd256] {
        let pcm = intersample(22_050);
        let mut sans = ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
        let mut sortie = Vec::new();
        sans.moduler(&pcm, &mut sortie).unwrap();
        assert!(sans.ecretages() > 0, "{taux:?} : contre-épreuve");
        let mut avec = ProducteurDsd::new(plan(TransportModulation::Native, taux, 44_100)).unwrap();
        avec.definir_gain_fixe(10f64.powf(MARGE_DU_MODULATEUR_DB / 20.0));
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

/// Relecture v2 (D1) : un producteur neuf en sourdine ne laisse passer
/// AUCUNE milliseconde à 0 dB (5 ms de continu pleine échelle en entrée).
#[test]
fn depart_en_sourdine_sans_bouffee_a_0_db() {
    let mut p =
        ProducteurDsd::new(plan(TransportModulation::Native, TauxDsd::Dsd64, 44_100)).unwrap();
    p.fixer_volume(0.0);
    let mut sortie = Vec::new();
    p.moduler(&vec![1.0; 2 * 220], &mut sortie).unwrap();
    let d = densite(&sortie, 0);
    assert!(
        d.abs() < 0.02,
        "densité des 5 premières ms : {d} (0,5 = 0 dB)"
    );
    // En cours de lecture, le changement passe par la rampe de 10 ms.
    p.definir_volume(1.0);
    let mut suite = Vec::new();
    p.moduler(&vec![1.0; 2 * 4_410], &mut suite).unwrap();
    let d = densite(&suite[suite.len() / 2..], 0);
    assert!((d - 0.5).abs() < 0.01, "{d}");
}

// ── Relecture v3 ────────────────────────────────────────────────────

/// Densité max (en valeur absolue) sur des fenêtres de `octets_fenetre`.
fn densite_max(octets: &[u8], octets_fenetre: usize) -> f64 {
    octets
        .chunks(octets_fenetre)
        .filter(|c| c.len() == octets_fenetre)
        .map(|c| densite(c, 0).abs())
        .fold(0.0, f64::max)
}

/// v3 D1 : seek au milieu d'un passage FORT, zone en sourdine. Le seek
/// construit un producteur neuf (`lien` = None quand seek_ms > 0) et lui
/// applique `fixer_volume` avant le premier bloc. Aucune fenêtre de 0,5 ms
/// ne doit porter de signal. Contre-épreuve : avec la rampe seule, la
/// première fenêtre est forte.
#[test]
fn relecture_v3_seek_dans_un_passage_fort_en_sourdine() {
    for transport in [TransportModulation::Native, TransportModulation::Dop] {
        let pl = plan(transport, TauxDsd::Dsd64, 44_100);
        // 50 Hz pleine échelle, pris au milieu (phase ≈ crête).
        let fort: Vec<f64> = sinus(44_100, 50.0, 1.0, 44_100)[2 * 220..2 * (220 + 441)].to_vec();
        let mut p = ProducteurDsd::new(pl).unwrap();
        p.definir_gain_fixe(10f64.powf(-3.0 / 20.0));
        p.fixer_volume(0.0);
        let mut s = Vec::new();
        p.moduler(&fort, &mut s).unwrap();
        let train = if transport == TransportModulation::Dop {
            dsd_du_dop(&s)
        } else {
            s
        };
        // 0,5 ms à DSD64 = 1411 bits/voie ≈ 176 octets/voie, 2 voies.
        let d = densite_max(&train, 352);
        assert!(
            d < 0.12,
            "{transport:?} : sourdine, fenêtre la plus forte {d}"
        );

        let mut r = ProducteurDsd::new(pl).unwrap();
        r.definir_gain_fixe(10f64.powf(-3.0 / 20.0));
        r.definir_volume(0.0);
        let mut s2 = Vec::new();
        r.moduler(&fort, &mut s2).unwrap();
        let train2 = if transport == TransportModulation::Dop {
            dsd_du_dop(&s2)
        } else {
            s2
        };
        let d2 = densite_max(&train2, 352);
        assert!(
            d2 > 0.2,
            "{transport:?} : contre-épreuve, rampe depuis 0 dB : {d2}"
        );
    }
}

/// v3 D2 : la piste en cours ne dépose JAMAIS (panne, blocage) : la
/// suivante rend la main au délai, sans état.
#[test]
fn relecture_v3_attente_bornee_si_la_precedente_ne_depose_jamais() {
    let zone = 9_430_901;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v3-a", pl);
    annoncer_la_suivante(zone, 2, "v3-a");
    let lien = prendre_le_lien(zone, Some(2), pl).unwrap();
    let t = std::time::Instant::now();
    let r = attendre_le_relais(&lien, pl, Duration::from_millis(300), &|| false);
    assert!(r.is_none());
    assert!(
        t.elapsed() < Duration::from_millis(700),
        "{:?}",
        t.elapsed()
    );
}

/// v3 D2 : abandon (stop, seek, relance de la piste en cours) pendant
/// l'attente : la suivante repart aussitôt, sans état.
#[test]
fn relecture_v3_abandon_pendant_l_attente() {
    let zone = 9_430_902;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v3-b", pl);
    annoncer_la_suivante(zone, 2, "v3-b");
    let lien = prendre_le_lien(zone, Some(2), pl).unwrap();
    let h = std::thread::spawn(move || {
        let t = std::time::Instant::now();
        let r = attendre_le_relais(&lien, pl, Duration::from_secs(20), &|| false);
        (r.is_none(), t.elapsed())
    });
    std::thread::sleep(Duration::from_millis(100));
    abandonner_le_relais("v3-b");
    let (aucun, duree) = h.join().unwrap();
    assert!(aucun);
    assert!(duree < Duration::from_secs(2), "{duree:?}");
}

/// v3 D2 : la session de la suivante est fermée (suivante retirée de la
/// file, re-préparation) : `annule` est vu en ≤ 200 ms.
#[test]
fn relecture_v3_annulation_de_la_suivante() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let zone = 9_430_903;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v3-c", pl);
    annoncer_la_suivante(zone, 2, "v3-c");
    let lien = prendre_le_lien(zone, Some(2), pl).unwrap();
    let ferme = std::sync::Arc::new(AtomicBool::new(false));
    let f2 = ferme.clone();
    let h = std::thread::spawn(move || {
        let t = std::time::Instant::now();
        let r = attendre_le_relais(&lien, pl, Duration::from_secs(20), &|| {
            f2.load(Ordering::SeqCst)
        });
        (r.is_none(), t.elapsed())
    });
    std::thread::sleep(Duration::from_millis(100));
    ferme.store(true, Ordering::SeqCst);
    let (aucun, duree) = h.join().unwrap();
    assert!(aucun);
    assert!(duree < Duration::from_secs(1), "{duree:?}");
    // L'état déposé ensuite par la piste en cours ne passe à personne
    // (aucune autre annonce).
    deposer_au_relais("v3-c", ProducteurDsd::new(pl).unwrap());
    assert!(prendre_le_lien(zone, Some(2), pl).is_none());
}

/// v3 D2 : la suivante a expiré (pause longue de la piste en cours) ; la
/// précédente dépose ENSUITE. Une re-préparation de la même suivante (même
/// piste, même session précédente) reprend-elle l'état ? (Lien légitime.)
/// Et une piste jouée à la main sans annonce ne le prend pas.
#[test]
fn relecture_v3_depot_apres_expiration() {
    let zone = 9_430_904;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v3-d", pl);
    annoncer_la_suivante(zone, 2, "v3-d");
    let lien = prendre_le_lien(zone, Some(2), pl).unwrap();
    assert!(attendre_le_relais(&lien, pl, Duration::from_millis(50), &|| false).is_none());
    deposer_au_relais("v3-d", ProducteurDsd::new(pl).unwrap());
    // Piste jouée à la main (pas d'annonce) : rien.
    assert!(prendre_le_lien(zone, Some(2), pl).is_none());
}

/// v3 D2 : deux attentes liées à la MÊME précédente (pré-armement refait
/// alors que l'ancienne session n'est pas encore fermée) : une seule reprend
/// l'état, l'autre repart neuve — jamais deux reprises du même état.
#[test]
fn relecture_v3_deux_attentes_une_seule_reprise() {
    let zone = 9_430_905;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v3-e", pl);
    annoncer_la_suivante(zone, 2, "v3-e");
    let l1 = prendre_le_lien(zone, Some(2), pl).unwrap();
    annoncer_la_suivante(zone, 2, "v3-e");
    let l2 = prendre_le_lien(zone, Some(2), pl).unwrap();
    let h1 = std::thread::spawn(move || {
        attendre_le_relais(&l1, pl, Duration::from_secs(5), &|| false).is_some()
    });
    let h2 = std::thread::spawn(move || {
        attendre_le_relais(&l2, pl, Duration::from_secs(5), &|| false).is_some()
    });
    std::thread::sleep(Duration::from_millis(100));
    deposer_au_relais("v3-e", ProducteurDsd::new(pl).unwrap());
    let (a, b) = (h1.join().unwrap(), h2.join().unwrap());
    assert!(a ^ b, "une seule reprise : {a} {b}");
}

/// v3 : relais tous EnCours (producteurs bloqués) : la table croît sans
/// borne (aucune éviction possible).
#[test]
fn relecture_v3_table_des_relais_bornee() {
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    for i in 0..20 {
        ouvrir_le_relais(9_430_906, &format!("v3-f-{i}"), pl);
    }
    // Bornée, même quand tous les producteurs sont bloqués (EnCours).
    assert!(relais_de_la_zone(9_430_906) <= RELAIS_PAR_ZONE);
    assert!(relais_tenus() <= RELAIS_MAX);
}

/// v3 point 1 : PURE verrouillé (volume tenu à 100 %), déverrouillé EN COURS
/// de piste par une écriture du réglage, puis volume baissé : la baisse
/// s'applique avant le modulateur, sans relire la base à chaque bloc.
#[test]
fn pure_deverrouille_en_cours_de_piste_puis_volume_baisse() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: std::sync::Arc<dyn crate::db::backend::DbBackend> = std::sync::Arc::new(db);
    let zone = 9_430_950;
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    reglages
        .set(
            &format!("zone_{zone}_audiophile"),
            r#"{"enabled":true,"lock_volume":true}"#,
        )
        .unwrap();
    publier_volume(zone, 1.0, false);
    rafraichir_reglages_de_volume(&db, zone);
    let v = volume_de_la_zone(zone);
    assert!(v.verrou_pure());
    let mut p =
        ProducteurDsd::new(plan(TransportModulation::Native, TauxDsd::Dsd64, 44_100)).unwrap();
    p.fixer_volume(v.gain());
    let mut s = Vec::new();
    // Sous verrou : une consigne à 50 % reste à 0 dB.
    publier_volume(zone, 0.5, false);
    p.definir_volume(v.gain());
    p.moduler(&vec![1.0; 2 * 4_410], &mut s).unwrap();
    assert!((densite(&s[s.len() / 2..], 0) - 0.5).abs() < 0.01);
    // Déverrouillé en cours de piste (écriture du réglage, n'importe quel
    // chemin) : le volume baissé s'applique au bloc suivant.
    reglages
        .set(&format!("zone_{zone}_audiophile"), r#"{"enabled":false}"#)
        .unwrap();
    assert!(!v.verrou_pure());
    p.definir_volume(v.gain());
    s.clear();
    p.moduler(&vec![1.0; 2 * 4_410], &mut s).unwrap();
    let d = densite(&s[s.len() / 2..], 0);
    assert!((d - 0.25).abs() < 0.01, "{d}");
    // Le trim aussi : −6,02 dB.
    reglages
        .set(&format!("zone_{zone}_gain_trim_db"), "-6.0206")
        .unwrap();
    p.definir_volume(v.gain());
    s.clear();
    p.moduler(&vec![1.0; 2 * 4_410], &mut s).unwrap();
    let d = densite(&s[s.len() / 2..], 0);
    assert!((d - 0.125).abs() < 0.01, "{d}");
}

/// v3 point 2 : la précédente AVANCE (elle joue sa fin) : la suivante,
/// même lue, attend son dépôt ; la précédente s'arrête d'avancer
/// (bloquée) : la suivante, lue, cède en ~`delai`.
#[test]
fn l_attente_suit_l_avance_de_la_precedente() {
    let zone = 9_430_960;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v3-g", pl);
    let fin = std::sync::Arc::new(AtomicBool::new(false));
    let f2 = fin.clone();
    // La précédente avance pendant 600 ms, puis dépose.
    let joue = std::thread::spawn(move || {
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(50));
            signaler_l_avance("v3-g");
        }
        deposer_au_relais("v3-g", ProducteurDsd::new(pl).unwrap());
        f2.store(true, Ordering::SeqCst);
    });
    let t = Instant::now();
    let r = attendre_le_relais_lue("v3-g", pl, Duration::from_millis(200), &|| false, &|| true);
    joue.join().unwrap();
    assert!(
        r.is_some(),
        "attendue tant que la précédente avance ({:?})",
        t.elapsed()
    );
    assert!(fin.load(Ordering::SeqCst));
    // Bloquée : plus aucune avance ; la suivante, lue, cède.
    ouvrir_le_relais(zone, "v3-h", pl);
    let t = Instant::now();
    let r = attendre_le_relais_lue("v3-h", pl, Duration::from_millis(200), &|| false, &|| true);
    assert!(r.is_none());
    assert!(
        t.elapsed() < Duration::from_millis(600),
        "{:?}",
        t.elapsed()
    );
    // Pas encore lue (pré-armement) : elle attend malgré l'inactivité.
    ouvrir_le_relais(zone, "v3-i", pl);
    let lue = std::sync::Arc::new(AtomicBool::new(false));
    let l2 = lue.clone();
    let h = std::thread::spawn(move || {
        attendre_le_relais_lue("v3-i", pl, Duration::from_millis(100), &|| false, &|| {
            l2.load(Ordering::SeqCst)
        })
        .is_some()
    });
    std::thread::sleep(Duration::from_millis(400));
    deposer_au_relais("v3-i", ProducteurDsd::new(pl).unwrap());
    assert!(
        h.join().unwrap(),
        "pré-armée et non lue : elle attend le dépôt"
    );
}

/// v4 D1 : une piste longue (ouverte depuis plus de `RELAIS_PERIME`, pauses
/// comprises) dont le producteur avance garde son relais : la suivante
/// pré-armée reprend bien son état, même après une purge.
#[test]
fn une_piste_longue_garde_son_relais() {
    let zone = 9_430_970;
    let pl = plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100);
    ouvrir_le_relais(zone, "v4-a", pl);
    vieillir_l_ouverture("v4-a", RELAIS_PERIME + Duration::from_secs(60));
    signaler_l_avance("v4-a");
    // Une autre ouverture déclenche la purge.
    ouvrir_le_relais(9_430_971, "v4-autre", pl);
    annoncer_la_suivante(zone, 2, "v4-a");
    let lien = prendre_le_lien(zone, Some(2), pl).expect("le relais de A est toujours là");
    deposer_au_relais("v4-a", ProducteurDsd::new(pl).unwrap());
    assert!(attendre_le_relais(&lien, pl, Duration::from_secs(2), &|| false).is_some());
}

/// v4 D2 : la longueur annoncée d'un flux DoP modulé est alignée sur une
/// PAIRE de trames (la sortie coupe sans reliquat de trame à la jonction).
#[test]
fn la_longueur_dop_annoncee_est_alignee_sur_une_paire_de_trames() {
    let p = ProducteurDsd::new(plan(TransportModulation::Dop, TauxDsd::Dsd64, 44_100)).unwrap();
    for ms in [1, 999, 1_001, 123_457, 600_007] {
        let e = p.entete(Some(ms));
        let n = u32::from_le_bytes(e[40..44].try_into().unwrap());
        assert_eq!(n % 12, 0, "{ms} ms : {n}");
        assert!(n > 0);
    }
    // Une heure de DoP DSD64 (3,8 Go) dépasse le WAV : en-tête de flux
    // continu, sans borne.
    let e = p.entete(Some(3_600_007));
    assert_eq!(u32::from_le_bytes(e[40..44].try_into().unwrap()), u32::MAX);
    // Durée inconnue : la sentinelle, jamais une borne.
    let e = p.entete(None);
    let n = u32::from_le_bytes(e[40..44].try_into().unwrap());
    assert!(n == 0x7FFF_FFFF - 36 || n == u32::MAX, "{n:#x}");
}

/// La marge du modulateur ne dépend PAS du headroom automatique de la chaîne
/// PCM (qui peut valoir 0 dB) : elle s'ajoute, une fois, au headroom de la
/// chaîne ; le modulateur n'applique le manuel que si le flux ne l'a pas
/// déjà fait (revue rc14, #9518 × #9520).
#[test]
fn la_marge_du_modulateur_est_la_sienne() {
    assert_eq!(headroom_avant_le_modulateur(0.0), -3.0);
    assert_eq!(headroom_avant_le_modulateur(-1.0), -4.0);
    assert_eq!(headroom_avant_le_modulateur(-12.0), -15.0);
    assert_eq!(headroom_avant_le_modulateur(2.0), -3.0);
    assert_eq!(headroom_avant_le_modulateur(f64::NAN), -3.0);
    assert_eq!(gain_fixe_du_modulateur_db(-12.0, true), -3.0);
    assert_eq!(gain_fixe_du_modulateur_db(-12.0, false), -15.0);
    assert_eq!(gain_fixe_du_modulateur_db(0.0, false), -3.0);
}
