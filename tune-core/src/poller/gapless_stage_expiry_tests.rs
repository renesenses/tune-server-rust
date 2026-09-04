use super::decisions::gapless_stage_expired;

/// Le cas de Progman : pause prise dans les 30 dernieres secondes d'un
/// morceau, donc APRES l'armement, puis une longue absence. Le flux ouvert
/// pour la piste suivante abandonne au bout de 300 s ; a la reprise, le
/// renderer va chercher une adresse morte et 0 octet part.
#[test]
fn a_long_pause_expires_the_staged_track() {
    assert!(gapless_stage_expired(true, Some(400)));
    assert!(gapless_stage_expired(true, Some(201)));
}

/// Une pause courte ne doit rien jeter : repreparer coute un transcodage
/// complet, inutile tant que le flux est encore vivant.
#[test]
fn a_short_pause_keeps_the_staged_track() {
    assert!(!gapless_stage_expired(true, Some(30)));
    assert!(!gapless_stage_expired(true, Some(200)));
}

/// Rien en attente : il n'y a rien a jeter, quel que soit le temps ecoule.
#[test]
fn nothing_staged_never_expires() {
    assert!(!gapless_stage_expired(false, Some(9_999)));
    assert!(!gapless_stage_expired(false, None));
}

/// Arme sans horodatage connu — le cas `gapless_skipped_exclusive_output`,
/// qui marque `gapless_sent` sans jamais renseigner l'instant. On ne doit
/// pas le rearmer en boucle a chaque tick.
#[test]
fn staged_without_a_timestamp_is_left_alone() {
    assert!(!gapless_stage_expired(true, None));
}

/// La marge sous le delai d'abandon du decodeur (300 s) doit rester : sans
/// elle, on rearmerait juste apres que le flux est mort, ou jamais.
#[test]
fn the_threshold_stays_below_the_decoder_timeout() {
    assert!(super::GAPLESS_STAGE_MAX_AGE_SECS < 300);
    assert!(super::GAPLESS_STAGE_MAX_AGE_SECS > 60);
}
