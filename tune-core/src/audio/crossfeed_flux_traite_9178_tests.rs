//! #9178 — `crossfeed_status` publiait `NonLocalOutput`, qui VERROUILLE le
//! contrôle, pour toute sortie ni locale ni réseau. Or OAAT et le navigateur
//! reçoivent le flux WAV progressif dont le relais porte le crossfeed
//! (mesuré sur la décision réelle : `orchestrator::tests::*_9178`). Pour
//! elles, le statut devient une réserve partielle qui laisse le réglage
//! actif ; les autres sorties non locales gardent le verrou.
use super::*;

/// Ni locale, ni réseau, hors PURE : ce que `crossfeed_status` rend pour
/// toute sortie OAAT, navigateur ou PULL.
fn non_locale(requested: bool) -> CrossfeedStatus {
    crossfeed_status(requested, false, false, false, false, false)
}

#[test]
fn oaat_et_navigateur_ne_verrouillent_plus_le_crossfeed_9178() {
    for (type_, appareil) in [
        (Some("oaat"), Some("oaat:salon")),
        (None, Some("oaat-group:maison")),
        (Some("browser"), Some("browser:onglet")),
    ] {
        assert!(
            sortie_porte_le_flux_traite(type_, appareil),
            "{type_:?} {appareil:?}"
        );
        let s = avec_le_flux_traite(
            non_locale(true),
            sortie_porte_le_flux_traite(type_, appareil),
        );
        assert!(
            !s.unavailable,
            "#9178 : {type_:?} reçoit le flux traité, le contrôle ne doit pas être verrouillé"
        );
        assert!(s.effective);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalPartial));
        assert!(s.detail.is_some_and(|d| d.contains("telles quelles")));
    }
}

/// Réglage non demandé : la réserve est publiée, rien n'est « effectif ».
#[test]
fn case_decochee_sur_oaat_rien_d_effectif_9178() {
    let s = avec_le_flux_traite(non_locale(false), true);
    assert!(!s.effective);
    assert!(!s.unavailable);
    assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalPartial));
}

/// Contre-épreuve : les sorties PULL et la zone sans sortie gardent le verrou.
#[test]
fn les_sorties_pull_et_la_zone_sans_sortie_restent_verrouillees_9178() {
    // AirPlay et Devialet AIR en sont sortis avec TESS T12c (voir
    // `airplay_et_devialet_air_portent_le_flux_traite_t12c`).
    for (type_, appareil) in [
        (Some("diretta"), Some("diretta:x")),
        (Some("hqplayer"), Some("hqplayer:x")),
        (Some("sendspin"), Some("sendspin:x")),
        (Some("tune_agent"), Some("agent:x")),
        (None, None),
    ] {
        let porte = sortie_porte_le_flux_traite(type_, appareil);
        assert!(!porte, "{type_:?}");
        let s = avec_le_flux_traite(non_locale(true), porte);
        assert!(s.unavailable, "{type_:?}");
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalOutput));
    }
}

/// PURE garde la priorité : aucun chemin ne lève ce verrou.
#[test]
fn le_mode_pure_prime_sur_oaat_9178() {
    let pure = crossfeed_status(true, false, false, true, false, false);
    let s = avec_le_flux_traite(pure, true);
    assert_eq!(s.reason, Some(CrossfeedConstraint::PureMode));
    assert!(s.unavailable);
}

/// L'effet s'entend : les DROITS redeviennent la vraie condition, comme pour
/// les motifs partiels des zones réseau.
#[test]
fn sur_oaat_la_licence_redevient_la_condition_9178() {
    let s = avec_les_droits(avec_le_flux_traite(non_locale(true), true), false, true);
    assert_eq!(s.reason, Some(CrossfeedConstraint::PremiumRequired));
    let s = avec_les_droits(avec_le_flux_traite(non_locale(true), true), true, true);
    assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalPartial));
    assert!(!s.unavailable);
}

#[test]
fn le_code_serialise_9178() {
    let v = serde_json::to_value(avec_le_flux_traite(non_locale(true), true)).unwrap();
    assert_eq!(v["reason"], "non_local_partial");
    assert_eq!(v["unavailable"], false);
}

/// GARDE DE SITE : la route du statut applique la réserve ENTRE la sortie et
/// les droits.
#[test]
fn la_route_applique_le_flux_traite_avant_les_droits_9178() {
    const ROUTE: &str = include_str!("../../../tune-server/src/routes/zones/dsp.rs");
    let debut = ROUTE
        .find("async fn crossfeed_status_de_zone(")
        .expect("la route du statut crossfeed existe");
    let corps = &ROUTE[debut..];
    let corps = &corps[..corps.find("\n}\n").expect("fin de la fonction")];
    let sortie = corps
        .find("crossfeed::crossfeed_status(")
        .expect("statut de sortie");
    let flux = corps
        .find("avec_le_flux_traite(")
        .expect("#9178 : la route doit appliquer avec_le_flux_traite");
    let droits = corps.find("avec_les_droits(").expect("droits");
    assert!(sortie < flux && flux < droits);
}

/// TESS T12a — OAAT porte TOUTE la chaîne : le refus « non mesuré » est
/// levé, le réglage est honoré tel quel, sans motif (comme une sortie locale).
#[test]
fn oaat_porte_toute_la_chaine_sans_motif_t12a() {
    for (type_, appareil) in [
        (Some("oaat"), Some("oaat:salon")),
        (None, Some("oaat-group:maison")),
    ] {
        assert!(sortie_porte_toute_la_chaine(type_, appareil), "{type_:?}");
        let s = avec_toute_la_chaine(non_locale(true), true);
        assert_eq!(s.reason, None, "T12a : plus de motif sur OAAT");
        assert!(s.effective && !s.unavailable);
        let s = avec_toute_la_chaine(non_locale(false), true);
        assert!(!s.effective && !s.unavailable && s.reason.is_none());
    }
}

/// Contre-épreuve T12a : le navigateur garde sa réserve partielle, les
/// sorties PULL leur verrou, et PURE la priorité.
#[test]
fn seule_oaat_porte_toute_la_chaine_t12a() {
    for (type_, appareil) in [
        (Some("browser"), Some("browser:onglet")),
        (Some("airplay2"), Some("airplay:salon")),
        (Some("devialet_air"), Some("devialet:x")),
        (Some("hqplayer"), Some("hqplayer:x")),
        (Some("diretta"), Some("diretta:x")),
        (None, None),
    ] {
        assert!(!sortie_porte_toute_la_chaine(type_, appareil), "{type_:?}");
    }
    let pure = crossfeed_status(true, false, false, true, false, false);
    assert_eq!(
        avec_toute_la_chaine(pure, true).reason,
        Some(CrossfeedConstraint::PureMode)
    );
    // Les droits viennent après : sans Premium, le motif est le droit.
    let s = avec_les_droits(avec_toute_la_chaine(non_locale(true), true), false, true);
    assert_eq!(s.reason, Some(CrossfeedConstraint::PremiumRequired));
}

/// TESS T12c — AirPlay 1 et 2 et Devialet AIR : Tune fabrique le flux, le
/// crossfeed y est porté par le relais, réserve partielle (DSD, ALAC servis
/// tels quels) qui ne verrouille pas le contrôle.
#[test]
fn airplay_et_devialet_air_portent_le_flux_traite_t12c() {
    for (type_, appareil) in [
        (Some("airplay"), Some("airplay-192.168.1.9-7000")),
        (Some("airplay2"), Some("airplay2:salon")),
        (Some("devialet_air"), Some("devialet:x")),
    ] {
        assert!(sortie_porte_le_flux_traite(type_, appareil), "{type_:?}");
        assert!(!sortie_porte_toute_la_chaine(type_, appareil), "{type_:?}");
        let s = avec_le_flux_traite(non_locale(true), true);
        assert!(s.effective && !s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalPartial));
    }
}

/// TESS T12c (décision du 23/11) — HQPlayer et l'autre transport hors TESS :
/// le crossfeed seul n'y est pas appliqué (motif propre `outside_tess`,
/// verrouillé, sans nag de licence) ; avec un traitement de zone conservé
/// (égaliseur, correction de pièce, ReplayGain), il passe par le flux traité :
/// réserve partielle, contrôle actif, `effective` vrai.
#[test]
fn sortie_hors_tess_crossfeed_seul_ou_avec_un_traitement_t12c() {
    let seul = avec_la_sortie_hors_tess(non_locale(true), true, false);
    assert_eq!(seul.reason, Some(CrossfeedConstraint::OutsideTess));
    assert!(seul.unavailable && !seul.effective);
    assert_eq!(CrossfeedConstraint::OutsideTess.code(), "outside_tess");
    assert_eq!(
        avec_les_droits(seul.clone(), false, false).reason,
        Some(CrossfeedConstraint::OutsideTess),
        "pas d'invitation au Premium pour un effet qui ne s'entend pas"
    );

    let avec = avec_la_sortie_hors_tess(non_locale(true), true, true);
    assert_eq!(avec.reason, Some(CrossfeedConstraint::NonLocalPartial));
    assert!(avec.effective && !avec.unavailable);

    // Contre-épreuves : une autre sortie non locale garde son motif, PURE garde
    // la priorité.
    assert_eq!(
        avec_la_sortie_hors_tess(non_locale(true), false, true).reason,
        Some(CrossfeedConstraint::NonLocalOutput)
    );
    let pure = crossfeed_status(true, false, false, true, false, false);
    assert_eq!(
        avec_la_sortie_hors_tess(pure, true, true).reason,
        Some(CrossfeedConstraint::PureMode)
    );
}

/// Le motif `non_local_output` sert à toutes les sorties qui ne reçoivent pas
/// le flux traité (Sendspin, AES67, greffons, zone sans périphérique) : son
/// texte ne dit ni « hors TESS » ni ne promet un transport précis.
#[test]
fn le_texte_de_non_local_output_reste_neutre_t12c() {
    let d = CrossfeedConstraint::NonLocalOutput.detail();
    for interdit in ["hors TESS", "HQPlayer", "AirPlay", "DLNA", "toujours"] {
        assert!(!d.contains(interdit), "{interdit} : {d}");
    }
    assert!(
        CrossfeedConstraint::OutsideTess
            .detail()
            .contains("hors TESS")
    );
}
