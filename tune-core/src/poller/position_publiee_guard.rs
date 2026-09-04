/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe à ce module pour que
/// les motifs cherchés ne puissent pas se trouver eux-mêmes dans les
/// messages d'assertion ci-dessous (#2082).
fn code_de_production() -> &'static str {
    const TOUT: &str = include_str!("../poller.rs");
    const BORNE: &str = "mod position_publiee_guard";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

#[test]
fn l_evenement_position_porte_la_valeur_retenue_par_la_garde() {
    let code = code_de_production();
    assert!(
        code.contains("let publiee = self.playback.update_position(zone_id, reported).await;"),
        "le sondeur doit RECUEILLIR la position retenue par la garde de \
         monotonie (#3229)."
    );
    assert!(
        code.contains("self.playback.emit_position(zone_id, publiee);"),
        "l'évènement `position` doit porter la valeur retenue, pas la valeur \
         brute du renderer : sinon l'écran recule quand même et l'état de \
         zone le contredit (#3229)."
    );
    assert!(
        !code.contains("self.playback.emit_position(zone_id, reported);"),
        "la valeur brute est revenue dans l'évènement `position` (#3229)."
    );
}
