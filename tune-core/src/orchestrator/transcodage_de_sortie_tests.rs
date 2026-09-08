//! #3183 — le bras Chromecast de `needs_transcode_for_output_applies`.
//!
//! La quatrieme condition partagee entre la decision et le miroir du chemin du
//! signal. Ces epreuves fixent le SEUL couple qui separe les deux bras — zone
//! `chromecast` + source AIFF — et les temoins qui l'entourent, pour qu'un
//! elargissement de `needs_transcode_for_dlna` ne passe pas inapercu.

use crate::audio::formats::AudioFormat;
use crate::orchestrator::needs_transcode_for_output_applies;

/// Le cas qui divergeait : le Default Media Receiver ne decode pas l'AIFF.
#[test]
fn un_aiff_sur_une_zone_chromecast_doit_etre_transcode() {
    assert!(needs_transcode_for_output_applies(
        Some("chromecast"),
        Some(AudioFormat::Aiff),
        false,
        false,
        false,
    ));
}

/// Contre-epreuve : le MEME AIFF sur une zone DLNA part direct. C'est ce que
/// le miroir repondait pour les deux, et c'est pour cela qu'il mentait.
#[test]
fn le_meme_aiff_sur_une_zone_dlna_part_direct() {
    assert!(!needs_transcode_for_output_applies(
        Some("dlna"),
        Some(AudioFormat::Aiff),
        false,
        false,
        false,
    ));
}

/// Les deux bras ne different QUE sur l'AIFF : tout autre format doit rendre
/// le meme verdict des deux cotes. Si un jour ils divergent ailleurs, cette
/// epreuve le dit au lieu de laisser la difference passer en silence.
#[test]
fn hors_aiff_les_deux_bras_sont_indiscernables() {
    for f in [
        AudioFormat::Flac,
        AudioFormat::Wav,
        AudioFormat::Mp3,
        AudioFormat::Aac,
        AudioFormat::Alac,
        AudioFormat::Ogg,
        AudioFormat::Opus,
        AudioFormat::Dsd,
        AudioFormat::WavPack,
        AudioFormat::Ape,
        AudioFormat::Wma,
    ] {
        assert_eq!(
            needs_transcode_for_output_applies(Some("chromecast"), Some(f), false, false, false),
            needs_transcode_for_output_applies(Some("dlna"), Some(f), false, false, false),
            "{f:?} : les deux bras doivent s'accorder hors AIFF"
        );
    }
}

/// Les trois passthroughs desarment la condition, sur les deux bras.
#[test]
fn un_passthrough_arme_desarme_le_transcodage() {
    for (dsd, alac, aac) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        assert!(!needs_transcode_for_output_applies(
            Some("chromecast"),
            Some(AudioFormat::Aiff),
            dsd,
            alac,
            aac,
        ));
    }
}

/// Hors sortie reseau, la condition est sans objet — y compris pour l'AIFF.
#[test]
fn hors_sortie_reseau_aucun_transcodage_de_sortie() {
    for t in [
        Some("local"),
        Some("browser"),
        Some("oaat"),
        Some("hqplayer"),
        None,
    ] {
        assert!(!needs_transcode_for_output_applies(
            t,
            Some(AudioFormat::Aiff),
            false,
            false,
            false,
        ));
    }
}

/// Sans format source, rien a decider.
#[test]
fn sans_format_source_la_condition_est_fausse() {
    assert!(!needs_transcode_for_output_applies(
        Some("chromecast"),
        None,
        false,
        false,
        false,
    ));
}
