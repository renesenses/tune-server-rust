//! #3973 — « bit-perfect strict » : les deux sites de la sortie locale.
//!
//! Site 1, l'ouverture cpal (`BackendCpal::ouvrir`) : seul le bras
//! `ResampleToDeviceRate` convertit ; strict ⇒ `RefusDOuverture::BitPerfectStrict`
//! avant qu'un flux soit construit. Site 2, le changement de cadence en cours
//! de flux (enchaînement gapless) : strict ⇒ on n'enchaîne pas, la piste
//! suivante repasse par l'ouverture.
//!
//! Aucun DAC plafonné n'est joignable depuis la machine de test : chaque site
//! a donc DEUX témoins — la décision qu'il consomme, éprouvée par valeurs, et
//! une garde du BRANCHEMENT qui lit le site réel (l'appel, sa place, ce qu'il
//! fait du refus), parce qu'une garde qui construit elle-même sa décision ne
//! garde pas le branchement.

use super::backend::{RefusDOuverture, refus_strict_a_l_ouverture};
use super::*;

/// La source sans aucun blanc : les gardes de branchement ne dépendent pas de
/// la mise en forme de `cargo fmt`.
fn compact(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn ouverture_3973_seul_le_reechantillonnage_est_refuse_et_seulement_en_strict() {
    let reech = LocalRateOpening::ResampleToDeviceRate {
        device_sample_rate: 96_000,
        reason: LocalRateFallback::RateNotSupported,
    };
    let refus = refus_strict_a_l_ouverture(reech, 192_000, true).expect("strict refuse");
    assert_eq!((refus.demandee_hz, refus.sortie_hz), (192_000, 96_000));
    assert_eq!(
        refus_strict_a_l_ouverture(reech, 192_000, false),
        None,
        "par défaut : jouer (et le dire)"
    );
    for nominal in [
        LocalRateOpening::DeviceAlreadyAtSourceRate,
        LocalRateOpening::AtSourceRateMeasured,
        LocalRateOpening::LastResortSourceRate,
    ] {
        assert_eq!(refus_strict_a_l_ouverture(nominal, 192_000, true), None);
    }
}

#[test]
fn ouverture_3973_le_refus_rapporte_la_sentinelle_au_sondeur() {
    let slot = std::sync::Mutex::new(None);
    RefusDOuverture::BitPerfectStrict(crate::audio::bitperfect_strict::RefusBitPerfect {
        demandee_hz: 192_000,
        sortie_hz: 96_000,
    })
    .rapporter("DAC USB", &slot);
    assert_eq!(
        slot.lock().unwrap().as_deref(),
        Some("bitperfect_strict_refused:192000:96000")
    );
}

/// Garde du BRANCHEMENT, site 1 : `ouvrir` consulte la règle avec le drapeau
/// de la DEMANDE, AVANT de choisir la configuration, et rend le refus.
#[test]
fn ouverture_3973_ouvrir_refuse_avant_de_construire_le_flux() {
    let src = compact(include_str!("backend.rs"));
    let debut = src
        .find(
            "fnouvrir(demande:&DemandeDOuverture<'a>)->Result<Self,RefusDOuverture>{letdevice_name",
        )
        .expect("`BackendCpal::ouvrir`");
    let ouvrir = &src[debut..];
    let appel = ouvrir
        .find("refus_strict_a_l_ouverture(decision,sample_rate,demande.strict_bitperfect)")
        .expect("`ouvrir` doit consulter la règle bit-perfect avec le drapeau de la demande");
    let choix = ouvrir
        .find("let(chosen,opened_sr,reason)=match(decision")
        .expect("le choix de configuration");
    assert!(
        appel < choix,
        "la règle doit trancher AVANT le choix de configuration"
    );
    assert!(
        ouvrir[appel..choix].contains("returnErr(RefusDOuverture::BitPerfectStrict(refus));"),
        "le refus doit être RENDU, pas seulement calculé"
    );
}

/// Garde du BRANCHEMENT, site 1 amont : `play_url` lit le réglage posé par
/// l'orchestrateur et le passe à la demande d'ouverture partagée.
#[test]
fn ouverture_3973_play_url_transmet_le_reglage_de_la_zone() {
    let src = compact(include_str!("../local.rs"));
    assert!(
        src.contains("letstrict_bitperfect=self.strict_bitperfect.load(Ordering::Relaxed);"),
        "play_url doit lire le réglage posé par l'orchestrateur"
    );
    let demande = src
        .find("letdemande=DemandeDOuverture{spec,")
        .expect("la demande d'ouverture partagée");
    let fin = src[demande..].find("};").unwrap() + demande;
    assert!(
        src[demande..fin].contains(",strict_bitperfect,"),
        "la demande d'ouverture partagée doit porter le réglage de la zone"
    );
}

/// L'orchestrateur pose le réglage de la zone sur la sortie à chaque lecture,
/// à côté de PURE ; la sortie le garde.
#[test]
fn ouverture_3973_l_orchestrateur_pose_le_reglage_a_chaque_lecture() {
    let src = compact(include_str!("../../orchestrator/transport.rs"));
    let pure = src
        .find("local_output.set_pure_bypass(zone_audiophile);")
        .expect("le bloc PURE de send_to_output");
    let apres = &src[pure..pure + 400];
    assert!(
        apres.contains(
            "local_output.set_strict_bitperfect(crate::audio::bitperfect_strict::zone_enabled(&self.db,zone_id),);"
        ),
        "send_to_output doit poser « bit-perfect strict » de la zone, comme PURE"
    );
    let sortie = LocalOutput::new("DAC USB".into());
    assert!(!sortie.strict_bitperfect_for_test(), "désarmé par défaut");
    sortie.set_strict_bitperfect(true);
    assert!(sortie.strict_bitperfect_for_test());
}

#[test]
fn enchainement_3973_strict_n_enchaine_pas_une_autre_cadence() {
    assert!(enchainement_refuse_par_le_strict(
        192_000, 96_000, true, "DAC USB"
    ));
    assert!(!enchainement_refuse_par_le_strict(
        192_000, 96_000, false, "DAC USB"
    ));
    assert!(!enchainement_refuse_par_le_strict(
        96_000, 96_000, true, "DAC USB"
    ));
}

/// Garde du BRANCHEMENT, site 2 : la boucle gapless consulte la règle AVANT de
/// décider du rééchantillonnage de la piste enchaînée, et en sort.
#[test]
fn enchainement_3973_la_boucle_gapless_sort_avant_de_convertir() {
    let src = compact(include_str!("../local.rs"));
    let appel = src
        .find("ifenchainement_refuse_par_le_strict(new_sr,output_sr,strict_bitperfect,&device_name,){break;}")
        .expect("la boucle gapless doit consulter la règle bit-perfect, et en sortir");
    let decision = src
        .find("letnext_needs_resample=output_sr!=new_sr;")
        .expect("la décision de rééchantillonner la piste enchaînée");
    assert!(
        appel < decision,
        "la règle doit trancher AVANT le rééchantillonnage"
    );
}
