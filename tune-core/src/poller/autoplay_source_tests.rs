use super::decisions::autoplay_prefers_streaming as prefers;

#[test]
fn une_ecoute_qobuz_cherche_dans_qobuz() {
    assert!(prefers(Some("qobuz")));
    assert!(prefers(Some("tidal")));
    assert!(prefers(Some("deezer")));
}

#[test]
fn une_ecoute_locale_reste_locale() {
    // Le generateur local garde la main quand c'est du local qui joue :
    // ce lot ne doit rien changer pour qui n'a pas d'abonnement.
    assert!(!prefers(Some("local")));
}

#[test]
fn une_source_absente_ou_vide_ne_bascule_rien() {
    // Sens de defaut : sans source identifiee, on ne va pas interroger un
    // service au hasard.
    assert!(!prefers(None));
    assert!(!prefers(Some("")));
}
