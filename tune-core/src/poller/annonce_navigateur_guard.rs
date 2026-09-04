/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe à ce module pour que
/// les motifs cherchés ne puissent pas se trouver eux-mêmes dans les
/// messages d'assertion ci-dessous (#2082).
fn code_de_production() -> &'static str {
    const TOUT: &str = include_str!("../poller.rs");
    const BORNE: &str = "mod annonce_navigateur_guard";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

fn position(motif: &str) -> usize {
    code_de_production().find(motif).unwrap_or_else(|| {
        panic!(
            "motif introuvable dans poller.rs : « {motif} ».\n\
             Le code a été remanié ; ce garde-fou ne garde plus rien tant \
             qu'il n'a pas suivi. Voir #1998."
        )
    })
}

/// L'appel doit vivre DANS la branche « pas de périphérique de sortie » :
/// après le rafraîchissement radio qui lui est propre, et avant le code
/// qui ne concerne que les zones AVEC périphérique.
#[test]
fn la_zone_sans_peripherique_libere_son_annonce_en_attente() {
    let branche_sans_peripherique = position("decisions::deviceless_radio_refresh_due(");
    let liberation = position(".confirmer_lecture_navigateur(zone_id, stream_id)");
    let apres_le_match = position("// Detect track change: if the generation changed");
    assert!(
        branche_sans_peripherique < liberation && liberation < apres_le_match,
        "l'annonce des zones navigateur n'est plus libérée dans la branche \
         « zone sans périphérique » : plus aucune zone navigateur ne \
         scrobblerait, sans le moindre message (#1998)."
    );
}

/// Garde-fou #2630, versant symétrique du précédent.
///
/// L'abandon d'une lecture que personne ne reçoit doit vivre dans la MÊME
/// branche : c'est la seule où l'état « en lecture » ne repose sur rien.
/// Retiré de la boucle, la méthode reste compilée, testée et verte — et
/// plus personne ne l'appelle. La zone 987 se remettrait à jouer dans le
/// vide, sans le moindre message.
#[test]
fn la_zone_sans_peripherique_renonce_a_ce_quelle_nenvoie_pas() {
    let branche_sans_peripherique = position("decisions::deviceless_radio_refresh_due(");
    let abandon = position(".abandonner_lecture_sans_destination(zone_state)");
    let apres_le_match = position("// Detect track change: if the generation changed");
    assert!(
        branche_sans_peripherique < abandon && abandon < apres_le_match,
        "l'abandon d'une lecture sans destination n'est plus appelé dans la \
         branche « zone sans périphérique » : une zone qui n'envoie rien \
         resterait annoncée « en lecture » indéfiniment (#2630)."
    );
}
