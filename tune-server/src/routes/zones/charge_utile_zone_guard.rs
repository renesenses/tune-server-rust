/// ⚠️ La source est tronquée AVANT ce module.
///
/// `include_str!` rend le fichier entier, module de test compris — et les
/// motifs cherchés ci-dessous y figurent mot pour mot. Un `contains` sur le
/// fichier complet se trouverait lui-même et rendrait vrai quoi qu'il
/// arrive. Vécu le jour même sur un autre garde-fou (#2082) : il avait
/// survécu au sabotage de la condition qu'il prétendait garder.
fn code_de_production() -> &'static str {
    const TOUT: &str = include_str!("../zones.rs");
    const BORNE: &str = "mod charge_utile_zone_guard";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("module renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

/// 🔴 Le point aveugle qui a laissé passer la troisième copie (#2055).
///
/// Ce garde-fou affirmait « TOUTE charge utile de zone » en ne lisant qu'un
/// seul fichier. La charge utile est pourtant construite dans deux : les
/// deux `obj.insert(…)` de `zones.rs`, et le `json!` de `build_zone_json`
/// (`playback.rs`) — celui que rendent une vingtaine de routes de lecture.
/// Cette troisième copie portait `queue_length`, `queue_position` et
/// `can_skip_next`, mais ni `shuffle` ni `repeat` : exactement la
/// divergence que ce contrôle prétendait interdire, un fichier plus loin.
///
/// On ne rend ici que le CORPS de `build_zone_json`. Le fichier entier
/// apporterait `Json(json!({ "shuffle": enabled }))` de `toggle_shuffle` et
/// son jumeau `"repeat"` de `toggle_repeat` — deux réponses qui ne décrivent
/// pas une zone — et les compteurs ne diraient plus rien.
fn corps_de_build_zone_json() -> &'static str {
    const TOUT: &str = include_str!("../playback.rs");
    const DEBUT: &str = "pub(crate) async fn build_zone_json(";
    const FIN: &str = "\nasync fn build_zone_json_with_result(";
    let debut = TOUT
        .find(DEBUT)
        .unwrap_or_else(|| panic!("`build_zone_json` renommée : la découpe ne garde plus rien"));
    let fin = TOUT[debut..]
        .find(FIN)
        .map(|i| debut + i)
        .unwrap_or_else(|| panic!("`build_zone_json_with_result` renommée : découpe perdue"));
    &TOUT[debut..fin]
}

/// `queue_length` sert de marqueur : c'est le champ que porte toute charge
/// utile décrivant l'état de lecture d'une zone. Chacune doit porter aussi
/// l'aléatoire, la répétition et la décision autoritaire « suivant ».
#[test]
fn toute_charge_utile_de_zone_porte_le_transport_et_la_decision_suivant() {
    // Les deux fichiers qui construisent la charge utile. Compter sur un
    // seul, c'était garder la moitié du code en croyant tout tenir (#2055).
    let src = format!("{}{}", code_de_production(), corps_de_build_zone_json());
    // Les motifs ne portent PAS le `obj.insert(` qui les précède : rustfmt
    // coupe un appel long sur trois lignes dès que ses arguments grossissent,
    // et le compteur retomberait alors à zéro sans qu'une seule charge utile
    // ait changé. Un garde-fou sensible à la mise en forme lâche en silence,
    // au pire moment — c'est la première version de celui-ci qui l'a montré.
    //
    // Deux écritures possibles pour la même clé : `"x".into()` dans un
    // `Map` (zones.rs) et `"x":` dans un `json!` (build_zone_json). Les
    // compter toutes les deux, sinon ajouter le champ dans la mauvaise
    // syntaxe laisserait le contrôle rouge sans faute — ou vert avec.
    let compter = |cle: &str| {
        src.matches(&format!(r#""{cle}".into()"#)).count()
            + src.matches(&format!(r#""{cle}":"#)).count()
    };
    let etats = compter("queue_length");
    let aleatoire = compter("shuffle");
    let repetition = compter("repeat");
    let suivant = compter("can_skip_next");

    assert!(
        etats >= 3,
        "le marqueur `queue_length` n'apparaît que {etats} fois — la forme \
         des charges utiles a changé, et ce contrôle ne garde plus rien. \
         Il en faut au moins TROIS : les deux de `zones.rs` et celle de \
         `build_zone_json` (#2055)."
    );
    assert_eq!(
        aleatoire, etats,
        "{etats} charge(s) utile(s) de zone, mais {aleatoire} portent \
         `shuffle` : une copie a divergé. Le client naîtrait de nouveau à \
         « aléatoire éteint » devant un serveur qui l'a activé (#2092)."
    );
    assert_eq!(
        repetition, etats,
        "{etats} charge(s) utile(s) de zone, mais {repetition} portent \
         `repeat` : même divergence, autre réglage."
    );
    assert_eq!(
        suivant, etats,
        "{etats} charge(s) utile(s) de zone, mais {suivant} portent \
         `can_skip_next` : le client recommencerait à deviner la fin de la \
         permutation depuis l'ordre brut de la file (#2337)."
    );
}
