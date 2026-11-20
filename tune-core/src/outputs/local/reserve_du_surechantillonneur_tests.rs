//! Décision de Bertrand du 19/11/2026 — la réserve du suréchantillonneur est
//! compensée dans le gain de rendu : niveaux égaux sous −3 dB de volume,
//! jamais d'écrêtage, rien de compensé à 100 %.
use super::super::{LocalOutput, UNITE_DU_GAIN_DE_RENDU};
use super::*;

/// La réserve d'usage de l'étage (`RESERVE_DB` du greffon).
const RESERVE_DB: f64 = -3.0;

/// `set_volume` sans l'exécuteur asynchrone : ce qu'il fait au gain.
fn regler_le_volume(sortie: &LocalOutput, volume: f64) {
    sortie
        .user_volume
        .store(super::super::unites_de_gain(volume), Ordering::SeqCst);
    sortie.recompute_effective_volume();
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// Le niveau qui part au DAC pour une crête source à pleine échelle : gain
/// de l'étage (s'il tourne) × gain de rendu que multiplient les rappels.
fn niveau_au_dac(sortie: &LocalOutput, etage: bool) -> f64 {
    let rendu = f64::from(sortie.gain_de_rendu().load(Ordering::SeqCst))
        / f64::from(UNITE_DU_GAIN_DE_RENDU);
    let gain_etage = if etage {
        10.0_f64.powf(RESERVE_DB / 20.0)
    } else {
        1.0
    };
    gain_etage * rendu
}

/// Le niveau au DAC avec puis sans l'étage, au même volume, même sortie.
fn avec_et_sans(sortie: &LocalOutput) -> (f64, f64) {
    sortie
        .reserve_du_surechantillonneur
        .poser_l_etage(Some(RESERVE_DB));
    let avec = niveau_au_dac(sortie, true);
    sortie.reserve_du_surechantillonneur.poser_l_etage(None);
    let sans = niveau_au_dac(sortie, false);
    (avec, sans)
}

#[test]
fn sous_moins_3_db_de_volume_les_niveaux_sont_egaux() {
    let sortie = LocalOutput::new("Réserve 19-11".to_string());
    for volume_db in [-3.0, -6.0, -20.0, -45.0] {
        regler_le_volume(&sortie, 10.0_f64.powf(volume_db / 20.0));
        let (avec, sans) = avec_et_sans(&sortie);
        assert!(
            (db(avec) - db(sans)).abs() < 0.005,
            "volume {volume_db} dB : avec {} dB, sans {} dB",
            db(avec),
            db(sans)
        );
    }
}

#[test]
fn avec_le_replaygain_les_niveaux_restent_egaux() {
    let sortie = LocalOutput::new("Réserve RG 19-11".to_string());
    regler_le_volume(&sortie, 0.5);
    sortie.set_replaygain_factor(0.6);
    let (avec, sans) = avec_et_sans(&sortie);
    assert!((db(avec) - db(sans)).abs() < 0.005, "{avec} / {sans}");
}

#[test]
fn le_gain_rendu_ne_depasse_jamais_l_unite_donc_n_ecrete_pas() {
    let sortie = LocalOutput::new("Rabot 19-11".to_string());
    sortie
        .reserve_du_surechantillonneur
        .poser_l_etage(Some(RESERVE_DB));
    for volume in [0.1, 0.5, 0.708, 0.8, 0.95, 1.0] {
        regler_le_volume(&sortie, volume);
        let rendu = sortie.gain_de_rendu().load(Ordering::SeqCst);
        assert!(rendu <= UNITE_DU_GAIN_DE_RENDU, "volume {volume} : {rendu}");
        // La sortie de l'étage est sous la pleine échelle (limiteur) : avec
        // un rendu ≤ 1, ce qui part au DAC aussi.
        assert!(niveau_au_dac(&sortie, true) <= 1.0 + 1e-9);
    }
    // Même avec un ReplayGain qui demande +6 dB.
    sortie.set_replaygain_factor(2.0);
    assert!(sortie.gain_de_rendu().load(Ordering::SeqCst) <= UNITE_DU_GAIN_DE_RENDU);
}

#[test]
fn a_100_pour_cent_rien_n_est_compense_et_le_chemin_le_dit() {
    let sortie = LocalOutput::new("100 % 19-11".to_string());
    regler_le_volume(&sortie, 1.0);
    let (avec, sans) = avec_et_sans(&sortie);
    assert!(
        (db(avec) - db(sans) - RESERVE_DB).abs() < 0.005,
        "{avec} / {sans}"
    );
    sortie
        .reserve_du_surechantillonneur
        .poser_l_etage(Some(RESERVE_DB));
    let ecart = sortie
        .reserve_du_surechantillonneur
        .ecart_de_niveau_db()
        .unwrap();
    assert!((ecart - RESERVE_DB).abs() < 0.005, "{ecart}");

    // Entre −3 et 0 dB : compensé en partie.
    regler_le_volume(&sortie, 10.0_f64.powf(-1.0 / 20.0));
    let ecart = sortie
        .reserve_du_surechantillonneur
        .ecart_de_niveau_db()
        .unwrap();
    assert!((ecart - -2.0).abs() < 0.005, "{ecart}");

    // Sous −3 dB : aucun écart.
    regler_le_volume(&sortie, 0.5);
    let ecart = sortie
        .reserve_du_surechantillonneur
        .ecart_de_niveau_db()
        .unwrap();
    assert!(ecart.abs() < 0.005, "{ecart}");

    // Sans étage, rien à dire.
    sortie.reserve_du_surechantillonneur.poser_l_etage(None);
    assert_eq!(
        sortie.reserve_du_surechantillonneur.ecart_de_niveau_db(),
        None
    );
}

#[test]
fn le_dop_garde_l_unite_exacte() {
    assert_eq!(
        effective_volume_units_avec(500_000, 1000, true, RESERVE_DB),
        UNITE_DU_GAIN_DE_RENDU
    );
}

fn effective_volume_units_avec(user: u32, rg: u32, dop: bool, reserve_db: f64) -> u32 {
    let gain = 10.0_f64.powf(reserve_db / 20.0);
    super::super::effective_volume_units(user, facteur_compense(rg, gain), dop)
}

#[test]
fn la_compensation_de_l_egaliseur_garde_sa_part_portee() {
    // #5227 — un facteur empaqueté (part portée par l'égaliseur en haut) :
    // seule la part ReplayGain × compensation reçoit `1 / gain`.
    let facteur = super::super::composer_le_facteur_de_rendu(1000, 2000);
    let compense = facteur_compense(facteur, 0.5);
    assert_eq!(
        super::super::decomposer_le_facteur_de_rendu(compense),
        (2000, 2000)
    );
}

#[test]
fn un_flux_neuf_remet_la_compensation_a_l_unite() {
    let sortie = LocalOutput::new("Remise 19-11".to_string());
    regler_le_volume(&sortie, 0.5);
    let sans = sortie.gain_de_rendu().load(Ordering::SeqCst);
    sortie
        .reserve_du_surechantillonneur
        .poser_l_etage(Some(RESERVE_DB));
    assert!(sortie.gain_de_rendu().load(Ordering::SeqCst) > sans);
    // Ce que fait `play_url` avant d'ouvrir quoi que ce soit.
    sortie.reserve_du_surechantillonneur.poser_l_etage(None);
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), sans);
    assert_eq!(sortie.gain_moyen_du_dsp().load(Ordering::SeqCst), 1000);
}

#[test]
fn le_cretemetre_reporte_la_reserve() {
    let sortie = LocalOutput::new("Crête 19-11".to_string());
    regler_le_volume(&sortie, 0.5);
    sortie
        .reserve_du_surechantillonneur
        .poser_l_etage(Some(RESERVE_DB));
    let gain = sortie.reserve_du_surechantillonneur.gain();
    // RMS et spectre : gain de rendu × gain moyen du DSP = volume seul.
    let rendu = f64::from(sortie.gain_de_rendu().load(Ordering::SeqCst))
        / f64::from(UNITE_DU_GAIN_DE_RENDU);
    let dsp = f64::from(sortie.gain_moyen_du_dsp().load(Ordering::SeqCst)) / 1000.0;
    assert!((db(rendu * dsp) - db(0.5)).abs() < 0.01, "{rendu} × {dsp}");
    // Crête : relevée avant l'étage, rangée avec la réserve.
    let cretes = sortie.cretes_de_sortie();
    cretes.poser_le_gain_aval(gain);
    cretes.relever(&[1.0, -1.0, 0.5, 0.5], 2, 48_000, 0.0);
    let (g, d) = cretes.crete_entre(0.0, 1.0).unwrap();
    assert!((db(g * rendu) - db(0.5)).abs() < 0.01, "{g}");
    assert!((db(d * rendu) - db(0.5)).abs() < 0.01, "{d}");
    // Un flux neuf l'oublie.
    cretes.oublier();
    cretes.relever(&[1.0, 1.0], 1, 48_000, 0.0);
    assert_eq!(cretes.crete_entre(0.0, 1.0), Some((1.0, 1.0)));
}

#[test]
fn le_chemin_du_signal_recoit_l_ecart() {
    use crate::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm, TransformationsReelles};
    let t = TransformationsReelles::nouvelles(
        AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).expect("spec valide"),
        FormatOuvert::new(352_800, 2),
        false,
    );
    // Sans étage, l'écart n'est jamais déclaré.
    assert_eq!(
        t.avec_ecart_de_niveau_surechantillonnage(-3.0)
            .ecart_de_niveau_surechantillonnage(),
        None
    );
    let t = t
        .avec_surechantillonnage("polyvalent", RESERVE_DB)
        .avec_ecart_de_niveau_surechantillonnage(-1.234);
    assert_eq!(t.ecart_de_niveau_surechantillonnage(), Some(-1.23));
}

/// Relecture du 20/11 (3) — un fil détaché (par `stop()` ou par un
/// `play_url` plus récent) qui ouvre son étage en retard ne réarme pas la
/// compensation du flux suivant.
#[test]
fn un_fil_detache_ne_rearme_pas_la_compensation() {
    let sortie = LocalOutput::new("Fil détaché 19-11".to_string());
    regler_le_volume(&sortie, 0.5);
    let sans = sortie.gain_de_rendu().load(Ordering::SeqCst);
    let reserve = &sortie.reserve_du_surechantillonneur;
    let generation = std::sync::atomic::AtomicU64::new(1);
    reserve.ouvrir_le_flux(1);
    // `stop()` a détaché le fil : la génération a avancé.
    generation.store(2, Ordering::SeqCst);
    assert!(!reserve.poser_l_ouverture(1, &generation, None, Some(RESERVE_DB)));
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), sans);
    // Un `play_url` plus récent a ouvert le flux 2 ; l'ancien fil arrive.
    reserve.ouvrir_le_flux(2);
    assert!(!reserve.poser_l_ouverture(1, &generation, None, Some(RESERVE_DB)));
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), sans);
    assert_eq!(reserve.gain(), 1.0);
    // Le fil du flux en cours, lui, arme la compensation.
    assert!(reserve.poser_l_ouverture(2, &generation, None, Some(RESERVE_DB)));
    assert!(sortie.gain_de_rendu().load(Ordering::SeqCst) > sans);
}
