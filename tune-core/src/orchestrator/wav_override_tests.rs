use super::wav_override_applies;

/// Le cas de Yves (#1437) : les deux cases cochées, une source FLAC.
/// Avant, le WAV gagnait en silence et le FLAC natif ne servait à rien.
#[test]
fn flac_source_with_native_flac_opt_in_keeps_flac() {
    assert!(!wav_override_applies(true, true, true));
}

/// La même zone, une source ALAC : le forçage WAV garde tout son sens,
/// c'est même sa raison d'être (décodeur ALAC du renderer).
#[test]
fn alac_source_still_goes_to_wav() {
    assert!(wav_override_applies(true, false, true));
}

/// Sans l'opt-in, une source FLAC suit le forçage comme avant — c'est ce
/// dont ont besoin les renderers qui ne lisent pas le FLAC.
#[test]
fn flac_source_without_opt_in_still_follows_the_override() {
    assert!(wav_override_applies(true, true, false));
}

/// Aucun forçage demandé : rien à neutraliser, dans les deux sens.
#[test]
fn no_override_requested_stays_off() {
    assert!(!wav_override_applies(false, true, true));
    assert!(!wav_override_applies(false, false, false));
}
