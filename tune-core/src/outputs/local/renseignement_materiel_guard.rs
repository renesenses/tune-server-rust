/// R6 (#2219) : l'énumération vit dans `local/parc.rs`, un module de
/// production sans aucun test — les motifs cherchés ne peuvent donc pas s'y
/// trouver eux-mêmes (#2082). Avant R6 on coupait `local.rs` à la
/// déclaration de ce module pour la même raison.
fn code_de_production() -> &'static str {
    include_str!("parc.rs")
}

#[test]
fn l_enumeration_garde_la_description_entiere() {
    assert!(
        code_de_production().contains("let description = device.description().ok();"),
        "l'énumération doit CONSERVER le `DeviceDescription` de cpal. Le \
         réduire à son `name()` est précisément ce qui jetait le nom du \
         contrôleur que Windows y met déjà (#2272)."
    );
}

#[test]
fn le_renseignement_est_calcule_sur_le_nom_brut_et_l_endpoint() {
    assert!(
        code_de_production()
            .contains("hardware_detail_from_description(desc, &raw_name, &endpoint_id)"),
        "la règle doit être nourrie du nom BRUT et de l'identifiant \
         d'endpoint. Lui passer le nom déjà désambiguïsé lui ferait \
         comparer le candidat à un suffixe « (2) » qui vient de nous, et \
         lui cacher que le PCM ALSA répète l'endpoint (#2272)."
    );
}

#[test]
fn le_peripherique_publie_porte_le_renseignement() {
    let code = code_de_production();
    let debut = code
        .find("devices.push(AudioDevice {")
        .expect("l'énumération ne construit plus l'AudioDevice qu'elle publie");
    let bloc = &code[debut..];
    let fin = bloc
        .find("});")
        .expect("le littéral AudioDevice de l'énumération n'est plus délimité");
    assert!(
        bloc[..fin].contains("hardware_detail,"),
        "l'énumération construit le périphérique publié SANS y porter le \
         renseignement matériel : la règle serait calculée puis jetée, et \
         Marco Polo verrait toujours deux « Haut-Parleurs » (#2272)."
    );
}
