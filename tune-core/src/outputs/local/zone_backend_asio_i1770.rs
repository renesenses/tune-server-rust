use super::LocalOutput;

/// Le cas mesuré chez jfpaquet le 02/09 en 0.9.130 :
/// `asio_device_not_found_listing_available requested=Speakers
/// available=["Essence STX II ASIO(64)"]`, deux fois en une minute, sans
/// aucun repli.
///
/// `exclusive_mode = true` n'est pas un choix de l'essai : sous ASIO,
/// `AppState::effective_exclusive_mode()` le rend vrai PAR CONSTRUCTION
/// (`config::exclusive_mode_status`), que la case soit cochée ou non.
#[test]
fn un_endpoint_wasapi_sous_asio_ne_part_pas_dans_le_chemin_asio() {
    let sortie = LocalOutput::with_options_and_endpoint(
        "Speakers".to_string(),
        Some("{0.0.0.00000000}.{a1b2c3d4}".to_string()),
        true,
        "asio",
    )
    .with_origin_host("WASAPI");

    assert_ne!(
        sortie.audio_backend(),
        "asio",
        "`AsioExclusiveOutput::new` ne reçoit un nom que par \
         `audio_backend == \"asio\"` : tant que cette sortie porte `asio`, \
         un nom WASAPI y est envoyé et ne peut qu'être refusé (#1770)"
    );
    assert_eq!(
        sortie.audio_backend(),
        "wasapi",
        "et `select_host` doit rouvrir l'hôte qui a énuméré ce nom, sans \
         quoi `resolve_device` refuse pour hôte étranger (#3230)"
    );
    assert_eq!(
        sortie.origin_host(),
        Some("WASAPI"),
        "l'étiquette d'origine reste posée : c'est elle qui arme le refus \
         de #3230 sur le chemin cpal"
    );
}

/// TÉMOIN — backend ASIO, périphérique ASIO réel : comportement INCHANGÉ.
///
/// C'est le périphérique de jfpaquet lui-même. Si cet essai tombe, le
/// correctif a désarmé ASIO au lieu de le protéger, et il n'y a plus de
/// lecture bit-perfect du tout.
#[test]
fn temoin_un_endpoint_asio_reel_reste_sur_le_chemin_asio() {
    let sortie = LocalOutput::with_options_and_endpoint(
        "Essence STX II ASIO(64)".to_string(),
        None,
        true,
        "asio",
    )
    .with_origin_host("ASIO");

    assert_eq!(
        sortie.audio_backend(),
        "asio",
        "un périphérique réellement énuméré par ASIO doit continuer de \
         partir dans la branche ASIO exclusive"
    );
    assert_eq!(sortie.origin_host(), Some("ASIO"));
}

/// Sans étiquette d'origine, rien n'est rectifié : on ne devine pas.
///
/// C'est le cas de `PlaybackOrchestrator::recreate_local_and_play`, qui
/// reconstruit une sortie à partir du seul `device_id`.
#[test]
fn sans_hote_d_origine_le_reglage_passe_tel_quel() {
    let sortie = LocalOutput::with_options_and_endpoint("Speakers".to_string(), None, true, "asio");
    assert_eq!(sortie.audio_backend(), "asio");
    assert_eq!(sortie.origin_host(), None);

    // Une chaîne vide vaut « inconnu », comme pour `origin_host`.
    let vide = LocalOutput::with_options_and_endpoint("Speakers".to_string(), None, true, "asio")
        .with_origin_host("");
    assert_eq!(vide.audio_backend(), "asio");
    assert_eq!(vide.origin_host(), None);
}

/// Le mode exclusif n'est PAS touché : il reste ce que
/// `effective_exclusive_mode()` a décidé. Une sortie WASAPI rectifiée part
/// donc dans le chemin WASAPI exclusif (`exclusive_mode && audio_backend
/// != "asio"`), qui lui, sait l'ouvrir.
#[test]
fn le_mode_exclusif_reste_celui_qu_on_a_recu() {
    let sortie = LocalOutput::with_options_and_endpoint("Speakers".to_string(), None, true, "asio")
        .with_origin_host("WASAPI");
    assert!(
        sortie.exclusive_mode,
        "rectifier le backend ne doit pas décider à la place de \
         l'utilisateur ce que vaut le mode exclusif"
    );
}
