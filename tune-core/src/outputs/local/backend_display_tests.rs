use super::backend_display_name;

// Le cas Bilou : l'utilisateur demande ASIO, le pilote n'est pas ouvrable
// (absent, ou déjà tenu par une autre application — un pilote ASIO ne
// s'ouvre que dans un seul processus), la lecture retombe sur WASAPI.
// L'interface annonçait quand même « ASIO ».
#[test]
fn observed_wins_over_requested() {
    assert_eq!(backend_display_name(Some("WASAPI"), "asio"), "WASAPI");
}

// Et l'inverse doit tenir aussi : une bascule vers WASAPI observée une fois
// ne doit pas figer l'affichage si ASIO s'ouvre ensuite.
#[test]
fn observed_asio_is_reported_even_when_setting_says_otherwise() {
    assert_eq!(backend_display_name(Some("ASIO"), "wasapi"), "ASIO");
}

// Sans observation — aucun périphérique encore ouvert — on retombe sur la
// déduction d'avant, inchangée.
#[test]
fn without_observation_falls_back_to_the_setting() {
    let name = backend_display_name(None, "asio");
    assert!(
        matches!(name, "ASIO" | "WASAPI" | "CoreAudio" | "ALSA" | "default"),
        "nom inattendu: {name}"
    );
}
