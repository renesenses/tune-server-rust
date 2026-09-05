/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe a ce module pour que
/// les motifs cherches ne puissent pas se trouver eux-memes dans les
/// assertions ci-dessous (#2082).
fn code_de_production() -> &'static str {
    static PRODUCTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRODUCTION.get_or_init(|| {
        const TOUT: &str = include_str!("../poller.rs");
        const BORNE: &str = "mod depassement_duree_nagit_pas_guard";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("ce module a ete renomme : la decoupe ne protege plus rien"));
        // `tick` vit dans son propre module (REF-1, #2219) et se lit en
        // premier : il précédait le reste de l'impl dans le fichier d'origine.
        format!("{}{}", include_str!("../poller/tick.rs"), &TOUT[..fin])
    })
}

fn position(motif: &str) -> usize {
    code_de_production().find(motif).unwrap_or_else(|| {
        panic!(
            "motif introuvable dans poller.rs : « {motif} ».\n\
             Le code a ete remanie ; ce garde-fou ne garde plus rien tant \
             qu'il n'a pas suivi. Voir #2493."
        )
    })
}

#[test]
fn le_bloc_de_constat_ne_touche_ni_la_piste_ni_la_zone() {
    let debut = position("decisions::position_au_dela_de_la_duree(");
    let fin = position("\"lecture_annoncee_au_dela_de_la_duree\"");
    assert!(
        debut < fin,
        "le journal du constat doit suivre l'appel au predicat"
    );
    let bloc = &code_de_production()[debut..fin];
    assert!(
        !bloc.contains("track_ended = true"),
        "le constat de depassement avance maintenant la piste. C'est \
         exactement ce que #2493 interdit : une duree FAUSSE produit la \
         meme forme qu'une lecture bloquee, et couper amputerait une \
         ecoute valide."
    );
    assert!(
        !bloc.contains("force_stop = true"),
        "le constat de depassement arrete maintenant la zone. C'est \
         exactement ce que #2493 interdit : une duree FAUSSE produit la \
         meme forme qu'une lecture bloquee, et couper amputerait une \
         ecoute valide."
    );
}

/// Le constat n'a le droit de parler qu'APRES que tous les detecteurs de
/// fin de piste ont renonce : il compte jusqu'a `DEPASSEMENT_DUREE_TICKS`,
/// pas jusqu'a `POSITION_PAST_END_TICKS`. Confondre les deux seuils ferait
/// crier le journal a chaque fin de piste normale.
#[test]
fn le_constat_attend_son_propre_seuil() {
    let debut = position("decisions::position_au_dela_de_la_duree(");
    let fin = position("\"lecture_annoncee_au_dela_de_la_duree\"");
    let bloc = &code_de_production()[debut..fin];
    assert!(
        bloc.contains("DEPASSEMENT_DUREE_TICKS"),
        "le constat ne s'appuie plus sur son propre seuil : il parlerait \
         avant que les detecteurs de fin de piste aient eu leur chance."
    );
    assert!(
        super::DEPASSEMENT_DUREE_TICKS > super::POSITION_PAST_END_TICKS,
        "le seuil du constat doit rester STRICTEMENT au-dessus de celui des \
         detecteurs de fin de piste, sinon il double-signale une fin de \
         piste parfaitement normale."
    );
}
