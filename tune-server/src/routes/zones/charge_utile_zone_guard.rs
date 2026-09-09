/// ⚠️ La source est tronquée AVANT ce module.
///
/// `include_str!` rend le fichier entier, module de test compris — et les
/// motifs cherchés ci-dessous y figurent mot pour mot. Un `contains` sur le
/// fichier complet se trouverait lui-même et rendrait vrai quoi qu'il
/// arrive. Vécu le jour même sur un autre garde-fou (#2082) : il avait
/// survécu au sabotage de la condition qu'il prétendait garder.
fn code_de_production() -> &'static str {
    static PRODUCTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRODUCTION.get_or_init(|| {
        const TOUT: &str = include_str!("../zones.rs");
        const BORNE: &str = "mod charge_utile_zone_guard";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("module renommé : la découpe ne protège plus rien"));
        // Les deux `obj.insert(…)` de la charge utile vivent dans le module
        // enfant `lecture` (`list_zones`, `get_zone`) depuis REF-4 (#2219) :
        // il se lit à la suite de zones.rs, avant sa borne.
        format!("{}{}", &TOUT[..fin], include_str!("lecture.rs"))
    })
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

/// 🔴 #2672 — LA GARDE. Les neuf réglages « Avancé · renderer » sortent des
/// TROIS charges utiles de zone.
///
/// SITES D'APPEL GARDÉS, nommément : `list_zones` et `get_zone`
/// (`routes/zones/lecture.rs`) et `build_zone_json` (`routes/playback.rs`) —
/// les trois appellent `crate::routes::zones::injecter_reglages_renderer`.
///
/// Le contrôle ne compte plus les clés une à une : elles ne sont plus écrites
/// qu'une fois, dans l'injecteur. Il compte les SITES D'APPEL, et les compare
/// au nombre de charges utiles que `queue_length` denombre déjà. Trois charges
/// utiles, trois appels — ou le contrôle tombe.
///
/// C'est la quatrième divergence de cette famille (#2055, #2092, #2337, puis
/// celle-ci) : à chaque fois, une copie à la main d'une charge utile de zone a
/// oublié un champ que les autres portaient.
#[test]
fn les_trois_charges_utiles_injectent_les_reglages_renderer() {
    let src = format!("{}{}", code_de_production(), corps_de_build_zone_json());

    let charges =
        src.matches(r#""queue_length".into()"#).count() + src.matches(r#""queue_length":"#).count();
    assert!(
        charges >= 3,
        "le marqueur `queue_length` n'apparaît que {charges} fois — la forme \
         des charges utiles a changé et ce contrôle ne garde plus rien."
    );

    let appels = src.matches("injecter_reglages_renderer(obj").count();
    assert_eq!(
        appels, charges,
        "{charges} charge(s) utile(s) de zone, mais {appels} appellent \
         `injecter_reglages_renderer` : une copie a divergé. Le client verrait \
         de nouveau les cases « Avancé · renderer » se décocher au lancement \
         d'un album (#2672)."
    );
}

/// 🔴 #2672 — l'autre moitié : l'injecteur écrit bien les NEUF clés.
///
/// Sans elle, la garde ci-dessus resterait verte devant un
/// `injecter_reglages_renderer` vidé de son corps : trois appels, zéro clé.
/// Elle APPELLE la fonction de production, sur une carte réelle, et lit ce
/// qu'elle y a mis — elle ne relit pas le source.
#[test]
fn l_injecteur_ecrit_les_neuf_reglages_du_panneau_avance() {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: std::sync::Arc<dyn tune_core::db::backend::DbBackend> = std::sync::Arc::new(db);
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(backend);
    let zone_id = repo.create("Salon", Some("dlna"), None).unwrap();

    // L'état que le testeur avait posé : trois cases cochées.
    repo.update_dlna_lpcm(zone_id, true).unwrap();
    repo.update_dlna_native_flac(zone_id, true).unwrap();
    repo.update_dlna_cap_16bit(zone_id, true).unwrap();

    let mut obj = serde_json::Map::new();
    crate::routes::zones::injecter_reglages_renderer(&mut obj, &repo, zone_id);

    for cle in [
        "dsd_mode",
        "lyrics_offset_ms",
        "dlna_native_flac",
        "alac_passthrough",
        "aac_passthrough",
        "dlna_lpcm",
        "dlna_cap_16bit",
        "dlna_wav24",
        "dlna_play_delay_ms",
    ] {
        assert!(
            obj.contains_key(cle),
            "la clé `{cle}` manque à la charge utile — c'est une case qui \
             se décoche à l'écran (#2672)"
        );
    }

    // Et ce sont les VALEURS de la base, pas des défauts : une garde qui
    // n'aurait vérifié que la présence resterait verte devant un injecteur
    // qui écrirait `false` partout — exactement le symptôme à empêcher.
    assert_eq!(obj["dlna_lpcm"], serde_json::json!(true));
    assert_eq!(obj["dlna_native_flac"], serde_json::json!(true));
    assert_eq!(obj["dlna_cap_16bit"], serde_json::json!(true));
    // Contre-épreuve : ce qui n'a pas été coché ne l'est pas.
    assert_eq!(obj["dlna_wav24"], serde_json::json!(false));
    assert_eq!(obj["alac_passthrough"], serde_json::json!(false));
}
