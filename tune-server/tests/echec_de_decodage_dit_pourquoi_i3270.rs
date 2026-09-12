//! #3270, point 1 — « la zone ne joue pas, et rien ne le dit ».
//!
//! Sur une zone LOCALE, un flux compressé (Bandcamp, podcast, UPnP, fichier
//! téléversé) est chargé en entier puis confié à symphonia. Quand le décodage
//! échouait, `tune-core/src/outputs/local.rs` posait un `warn!`, un drapeau à
//! `false` et un `return` nu : `open_failure` restait vide, donc
//! `take_output_failure()` ne rendait rien, donc le sondeur n'émettait aucun
//! `zone.playback_error`, donc l'écran ne recevait jamais de motif.
//!
//! ## Pourquoi cette garde vit ICI, dans `tune-server`
//!
//! `tune-core/src/outputs/local.rs` est derrière `#[cfg(feature =
//! "local-audio")]` (`tune-core/src/outputs/mod.rs:18`). Or le job `Test` de
//! la CI compile `tune-core` avec `--no-default-features --features
//! oaat,cloud-relay,bandcamp` (`.github/workflows/ci.yml:220`) : **sans
//! `local-audio`**. Les deux jobs qui l'activent — `test-shipped-features`
//! (`:257`) et `audio-embedding` (`:307`) — sont conditionnés à
//! `needs.impact.outputs.full == 'true'`, donc muets sur une PR vers
//! `batch/*`. Une épreuve unitaire posée dans `local.rs` ne tournerait jamais
//! sur la PR qui la livre (#1427, #2865, #3266).
//!
//! `include_str!` lit le TEXTE du fichier, quelles que soient les `cfg` : la
//! garde de site s'exécute dans `server_contracts`, que le job `Test` joue à
//! chaque PR. Les épreuves de COMPORTEMENT vivent, elles, dans
//! `local.rs::decode_failure_tests` — complémentaires, pas redondantes.
//!
//! Contre-épreuve : remettre le `return` nu sans alimenter `open_failure` fait
//! tomber `l_echec_de_decodage_alimente_le_canal_du_sondeur`.

const LOCAL: &str = include_str!("../../tune-core/src/outputs/local.rs");

/// Le bras d'échec du décodage, du site d'appel jusqu'au premier `return;`.
fn bras_d_echec_du_decodage() -> &'static str {
    let debut = LOCAL
        .find("decode_compressed_stream(&all_data)")
        .expect("le site d'appel du décodeur a disparu de local.rs");
    let reste = &LOCAL[debut..];
    let fin = reste
        .find("return;")
        .expect("le chemin d'échec du décodage ne rend plus la main");
    &reste[..fin]
}

/// LA garde. Le bras qui abandonne la lecture doit écrire dans `open_failure`,
/// sinon la zone s'arrête en silence.
#[test]
fn l_echec_de_decodage_alimente_le_canal_du_sondeur() {
    let bras = bras_d_echec_du_decodage();
    assert!(
        bras.contains("record_compressed_decode_failure"),
        "le bras d'échec du décodage ne nomme plus son motif ; \
         la zone s'arrêterait sans que l'écran sache pourquoi :\n{bras}"
    );
    assert!(
        bras.contains("open_failure"),
        "le bras d'échec du décodage n'alimente plus `open_failure`, \
         le canal que `take_output_failure()` draine :\n{bras}"
    );
}

/// TÉMOIN : le bras coupe bien la lecture. Une garde qui laisserait passer un
/// `playing` resté à `true` protégerait le message et pas le silence.
#[test]
fn le_bras_d_echec_arrete_aussi_la_lecture() {
    let bras = bras_d_echec_du_decodage();
    assert!(
        bras.contains("playing.store(false"),
        "le bras d'échec doit toujours baisser le drapeau de lecture :\n{bras}"
    );
}

/// Le motif doit être NOMMÉ, pas générique : quatre causes, quatre événements
/// de journal distincts.
#[test]
fn les_quatre_causes_de_decodage_ont_chacune_leur_evenement() {
    for evenement in [
        "local_audio_decode_container_unrecognised",
        "local_audio_decode_no_audio_track",
        "local_audio_decode_codec_unsupported",
        "local_audio_decode_no_samples",
    ] {
        assert!(
            LOCAL.contains(evenement),
            "l'événement `{evenement}` a disparu : une cause de décodage \
             a perdu son nom"
        );
    }
}

/// Le rapporteur doit écrire dans le verrou qu'on lui passe. Sans cette
/// écriture, l'appel du bras ci-dessus serait décoratif.
#[test]
fn le_rapporteur_de_decodage_ecrit_bien_dans_le_verrou() {
    let debut = LOCAL
        .find("fn record_compressed_decode_failure")
        .expect("le rapporteur d'échec de décodage a disparu de local.rs");
    let corps = &LOCAL[debut..(debut + 1200).min(LOCAL.len())];
    assert!(
        corps.contains("failure_slot.lock()"),
        "le rapporteur ne prend plus le verrou :\n{corps}"
    );
    assert!(
        corps.contains("*slot = Some("),
        "le rapporteur ne pose plus de message :\n{corps}"
    );
}

/// La famille `record_*` compte CINQ membres : refus PCM exclusif Windows,
/// refus d'ouverture exclusive, blocage d'alimentation (#3108), échec de
/// décodage (#3270), et désormais **périphérique introuvable sur le chemin
/// PARTAGÉ** — `record_shared_device_not_found`, qui couvre les deux jumeaux
/// `audio_device_not_found_no_fallback` (chemin WAV) et
/// `audio_device_not_found_compressed`.
///
/// Ce compte est le garde-fou du recensement : si un sixième silence trouve
/// son canal, il se déclare ici. C'est cette garde qui a fait rougir la PR
/// du chemin partagé, et c'est exactement son travail.
#[test]
fn la_famille_des_rapporteurs_compte_ses_membres() {
    let membres = LOCAL.matches("\nfn record_").count();
    assert_eq!(
        membres, 5,
        "la famille `record_*` de local.rs compte {membres} membre(s) et non 5 ; \
         mettre ce compte à jour EN NOMMANT le nouveau canal"
    );
}
