//! #4176 — deux bascules PURE sur dix-sept ont RELANCÉ le flux, et l'une a
//! arrêté la zone (`0x8889000A`, collision de deux ouvertures exclusives).
//!
//! Deux défauts, deux gardes :
//! 1. `refresh_zone_pure_dsp` lisait « pas de format » comme « rien en cours »
//!    alors que le fil venait d'être lancé et attendait son premier octet : la
//!    bascule est désormais REPORTÉE au flux qui démarre, sans relance.
//! 2. Le fil de lecture, sorti d'une première lecture HTTP bloquante de 10 s,
//!    ouvrait le périphérique sans regarder si `stop()` était passé : il
//!    perdait la course, écrivait l'échec dans le créneau partagé, et le
//!    sondeur arrêtait la zone du flux SUIVANT. Il vérifie avant d'ouvrir, et
//!    un fil périmé ne rapporte plus son échec.

use super::*;

/// La règle pure : dès que `stop()` est passé (silence forcé ou canal d'arrêt),
/// on n'ouvre plus.
#[test]
fn la_regle_refuse_l_ouverture_des_que_stop_est_passe_4176() {
    assert!(ouverture_encore_voulue(false, false));
    assert!(!ouverture_encore_voulue(true, false), "silence forcé");
    assert!(!ouverture_encore_voulue(false, true), "canal d'arrêt");
    assert!(!ouverture_encore_voulue(true, true));
}

/// Le témoin du trou : fil lancé, pas de format ⇒ « en démarrage », ni « rien
/// en cours » (avant le lancement) ni « ouvert » (format connu).
#[test]
fn un_flux_lance_sans_format_est_en_demarrage_4176() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    assert!(!sortie.flux_en_demarrage(), "rien de lancé : rien en cours");
    sortie.playing.store(true, Ordering::SeqCst);
    assert!(
        sortie.flux_en_demarrage(),
        "lancé et pas encore ouvert : c'est le trou dans lequel la bascule tombait"
    );
    sortie
        .current_format
        .store(LocalOutput::pack_format(44_100, 2), Ordering::Relaxed);
    assert!(
        !sortie.flux_en_demarrage(),
        "ouvert : la bascule à chaud s'applique comme avant"
    );
}

/// La garde du BRANCHEMENT dans `refresh_zone_pure_dsp` : un flux qui démarre
/// reçoit `pure_bypass` et la fonction rend `true` (« traité ») AVANT le
/// `return false` qui déclenche la relance.
#[test]
fn une_bascule_pure_pendant_le_demarrage_ne_relance_pas_le_flux_4176() {
    let src = include_str!("../../orchestrator/dsp.rs");
    let debut = src
        .find("pub async fn refresh_zone_pure_dsp")
        .expect("refresh_zone_pure_dsp");
    let seg = &src[debut..debut + 4_000];
    let garde = seg
        .find("local_output.flux_en_demarrage()")
        .expect("le flux en démarrage doit être reconnu");
    let apres = &seg[garde..];
    assert!(apres.contains("local_output.set_pure_bypass(pure)"));
    let traite = apres.find("return true;").unwrap();
    let relance = apres.find("return false;").unwrap();
    assert!(
        traite < relance,
        "le flux qui démarre est TRAITÉ avant le « rien en cours » qui relance"
    );
}

/// La garde du BRANCHEMENT dans le fil de lecture : après la première lecture
/// HTTP et avant tout bras exclusif, `ouverture_encore_voulue` est consultée.
#[test]
fn le_fil_verifie_l_arret_avant_d_ouvrir_le_peripherique_4176() {
    let src = include_str!("../local.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    let premiere_lecture = prod
        .find("\"local_audio_first_read\"")
        .expect("la première lecture HTTP");
    let apres = &prod[premiere_lecture..];
    let garde = apres
        .find("if !ouverture_encore_voulue(")
        .expect("la garde d'arrêt avant ouverture");
    let bras = apres
        .find("bras_wasapi::jouer_via_wasapi(")
        .expect("le bras WASAPI exclusif");
    assert!(garde < bras, "la garde doit précéder l'ouverture exclusive");
}

/// Un fil périmé (génération dépassée) ne rapporte pas son échec d'ouverture
/// et n'éteint pas `playing` : les deux bras comparent la génération AVANT
/// `rapporter`.
#[test]
fn un_fil_perime_ne_rapporte_pas_son_echec_d_ouverture_4176() {
    for (nom, src) in [
        ("bras_wasapi.rs", include_str!("bras_wasapi.rs")),
        ("bras_asio.rs", include_str!("bras_asio.rs")),
    ] {
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let ouvrir = prod
            .find("::ouvrir(&demande) {")
            .unwrap_or_else(|| panic!("{nom} : l'ouverture"));
        let bloc = &prod[ouvrir..ouvrir + 1_200];
        let generation = bloc
            .find("play_generation.load(Ordering::SeqCst) == my_generation")
            .unwrap_or_else(|| panic!("{nom} : la comparaison de génération"));
        let rapport = bloc
            .find("refus.rapporter(")
            .unwrap_or_else(|| panic!("{nom} : le rapport d'échec"));
        assert!(
            generation < rapport,
            "{nom} : la génération se compare AVANT de rapporter"
        );
    }
}
