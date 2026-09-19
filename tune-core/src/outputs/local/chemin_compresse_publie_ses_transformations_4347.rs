//! #4347 — WASAPI partagé, périphérique à 192 kHz : l'étape « 44kHz → 192kHz
//! (mesuré) » n'apparaissait qu'après une bascule PURE et ne disparaissait plus.
//!
//! Les deux états venaient de DEUX chemins de sortie : la radio décodée en
//! local (chemin compressé) rééchantillonne vers la cadence du périphérique
//! mais ne publiait aucune transformation — panneau « Sans perte », faux ; la
//! relance passait par le chemin PCM, qui publie — panneau « 44 → 192 kHz »,
//! vrai. Le chemin compressé publie désormais comme l'autre, avec la même
//! règle de DSP mesuré.

use super::*;
use std::sync::atomic::AtomicBool;

fn verrou<T>(v: Option<T>) -> std::sync::Mutex<Option<T>> {
    std::sync::Mutex::new(v)
}

/// La règle unique du DSP mesuré : DoP ou PURE éteignent tout ; sinon
/// égaliseur/convolveur toujours, crossfeed/mono en stéréo seulement.
#[test]
fn la_regle_du_dsp_mesure_est_celle_des_deux_chemins_4347() {
    let non = AtomicBool::new(false);
    let oui = AtomicBool::new(true);
    let eq_absent = verrou(None);
    let conv_absent = verrou(None);
    let cf_absent = verrou(None);
    assert!(
        !dsp_touche_le_signal(2, &non, &non, &eq_absent, &conv_absent, &cf_absent, &non),
        "rien d'armé : rien de touché"
    );
    assert!(
        dsp_touche_le_signal(2, &non, &non, &eq_absent, &conv_absent, &cf_absent, &oui),
        "repli mono en stéréo : touché"
    );
    assert!(
        !dsp_touche_le_signal(6, &non, &non, &eq_absent, &conv_absent, &cf_absent, &oui),
        "repli mono hors stéréo : `apply_local_dsp` ne l'applique pas"
    );
    assert!(
        !dsp_touche_le_signal(2, &non, &oui, &eq_absent, &conv_absent, &cf_absent, &oui),
        "PURE : rien ne touche le signal, quoi qu'il y ait d'armé"
    );
    assert!(
        !dsp_touche_le_signal(2, &oui, &non, &eq_absent, &conv_absent, &cf_absent, &oui),
        "DoP : idem"
    );
}

/// Le témoin du BRANCHEMENT : entre `local_audio_compressed_playing` et
/// `local_audio_compressed_stopped`, le chemin compressé écrit
/// `TransformationsReelles::nouvelles(...)` dans le créneau que
/// `transformations_reelles()` relit — avant le correctif, ce créneau restait
/// `None` sur tout ce chemin et le panneau n'avait rien à dire.
#[test]
fn le_chemin_compresse_publie_ses_transformations_a_l_ouverture_4347() {
    let src = include_str!("../local.rs");
    let debut = src
        .find("\"local_audio_compressed_playing\"")
        .expect("le chemin compressé joue");
    let fin = debut
        + src[debut..]
            .find("\"local_audio_compressed_stopped\"")
            .expect("le chemin compressé s'arrête");
    let chemin = &src[debut..fin];
    assert!(
        chemin.contains("transformations_reelles.lock()"),
        "le créneau des transformations doit être écrit sur ce chemin"
    );
    assert!(
        chemin.contains("TransformationsReelles::nouvelles("),
        "avec de vraies transformations (entrée, format ouvert, DSP)"
    );
    assert!(
        chemin.contains("FormatOuvert::new(output_sr, output_ch)"),
        "le format OUVERT, pas celui de la source : c'est lui qui dit le 192 kHz"
    );
    assert!(
        chemin.contains("dsp_touche_le_signal("),
        "la même règle de DSP mesuré que le chemin PCM"
    );
}

/// L'autre chemin n'a pas changé de règle : `EtageDeConversion::dsp_actif`
/// délègue à la fonction partagée — une seule définition du « DSP mesuré ».
#[test]
fn le_chemin_pcm_partage_la_meme_regle_4347() {
    let src = include_str!("../local.rs");
    let debut = src
        .find("fn dsp_actif(&self) -> bool {")
        .expect("dsp_actif");
    let corps = &src[debut..debut + 400];
    assert!(corps.contains("dsp_touche_le_signal("));
}
