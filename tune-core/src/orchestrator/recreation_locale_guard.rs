/// Le CORPS de `recreate_local_and_play`, de sa signature à son accolade
/// fermante.
///
/// ⚠️ Le fichier est inclus en ENTIER, ce module compris, et les motifs
/// cherchés figurent aussi dans les messages ci-dessous : un
/// `contains` sur le fichier complet se trouverait lui-même et rendrait
/// vrai quoi qu'il arrive (#2082). La découpe l'empêche — `find` rend la
/// PREMIÈRE occurrence, celle de la variante `local-audio`, la seule qui
/// construise réellement une sortie.
fn corps_de_recreation() -> &'static str {
    const TOUT: &str = include_str!("../orchestrator.rs");
    const SIGNATURE: &str = "    async fn recreate_local_and_play(\n";
    let debut = TOUT.find(SIGNATURE).unwrap_or_else(|| {
        panic!(
            "`recreate_local_and_play` a été renommée ou remaniée : ce \
             garde-fou ne garde plus rien tant qu'il n'a pas suivi (#1770)."
        )
    });
    let apres = &TOUT[debut..];
    let fin = apres
        .find("\n    }\n")
        .map(|i| i + 7)
        .unwrap_or(apres.len());
    let corps = &apres[..fin];
    assert!(
        corps.contains("LocalOutput"),
        "la découpe ne tombe plus sur la variante qui construit la sortie"
    );
    corps
}

#[test]
fn la_sortie_recreee_ne_code_pas_les_reglages_en_dur() {
    let corps = corps_de_recreation();
    assert!(
        corps.contains("self.reglages_sortie_locale()"),
        "`recreate_local_and_play` ne lit plus les réglages. Sous Windows \
         et macOS, un DAC éteint au démarrage (ou retiré par le balayage à \
         chaud) sortirait en PARTAGÉ et jamais en ASIO au premier appui \
         sur Lecture, sans que l'écran le dise (#1770)."
    );
    assert!(
        !corps.contains("LocalOutput::new("),
        "`LocalOutput::new` code `exclusive_mode = false` et \
         `audio_backend = \"auto\"` en dur : c'est exactement le défaut du \
         point 3 de #1770. Passer par `with_options` avec les valeurs lues."
    );
    assert!(
        corps.contains("exclusive_mode,") && corps.contains("&audio_backend,"),
        "les valeurs lues ne sont plus celles remises au constructeur : \
         les lire pour ne pas s'en servir ne corrige rien (#1770)."
    );
    for litteral in ["\"auto\"", "\"wasapi\"", "\"asio\"", "\"coreaudio\""] {
        assert!(
            !corps.contains(litteral),
            "`{litteral}` est de retour en dur dans \
             `recreate_local_and_play` : le réglage de l'utilisateur \
             redevient sans effet sur ce chemin (#1770)."
        );
    }
}
