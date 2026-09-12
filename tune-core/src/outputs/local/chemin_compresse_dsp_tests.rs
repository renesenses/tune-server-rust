/// #1725 — le chemin compresse ne passait par AUCUN DSP.
///
/// Les trois appels d'`apply_local_dsp` vivaient tous sur le chemin PCM.
/// Un flux non-WAV — FLAC, MP3, AAC decode en bloc — alimentait le tampon
/// sans egaliseur, sans correction de piece et sans crossfeed. Quatrieme
/// trou de la meme famille que #1216 (passthrough reseau), #1168
/// (navigateur) et Diretta (sortie pull) : un DSP annonce comme applique,
/// absent d'un chemin donne.
///
/// Ce test lit le CONTENU du fichier : il verifie que la branche
/// compressee applique la chaine AVANT de reechantillonner. C'est un
/// controle grossier, mais il attrape la seule regression qui compte —
/// quelqu'un qui deplace ou supprime cet appel.
#[test]
fn la_branche_compressee_applique_le_dsp_avant_le_reechantillonnage() {
    let source = include_str!("../local.rs");
    let branche = source
        .split("local_audio_compressed_playing")
        .nth(1)
        .expect("branche compressee introuvable");
    // On ne regarde que jusqu'au pre-remplissage du tampon.
    let avant_tampon = branche
        .split("Pre-fill the ring buffer")
        .next()
        .expect("pre-remplissage introuvable");

    let pos_dsp = avant_tampon
        .find("apply_local_dsp(")
        .expect("le chemin compresse n'applique AUCUN DSP (#1725)");
    // Le chemin compresse appelle `rubato_resample_track` depuis #2246 ;
    // le prefixe couvre les deux noms si la variante venait a changer.
    let pos_resample = avant_tampon.find("rubato_resample_");

    if let Some(pos_resample) = pos_resample {
        assert!(
            pos_dsp < pos_resample,
            "le DSP doit s'appliquer AVANT le reechantillonnage : \
             l'EqProcessor est bati pour (media.sample_rate, media.channels), \
             donc pour dec_sr/dec_ch. L'appliquer apres deplacerait toutes \
             les frequences de coupure."
        );
    }
}
