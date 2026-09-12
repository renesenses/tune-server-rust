use super::{
    LocalBackendFallback, ObservedBackend, ObservedDevice, asio_available, asio_outcome,
    backend_status, platform_default_backend_name, unsupported_outcome,
};

fn observed(name: &'static str, reason: Option<LocalBackendFallback>) -> Option<ObservedBackend> {
    Some(ObservedBackend {
        name,
        fallback_reason: reason,
    })
}

/// Une ouverture de périphérique observée : ce qui était demandé, ce qui a
/// été ouvert. `opened_id` optionnel — ASIO et CoreAudio n'en ont pas.
fn ouvert(
    backend: &'static str,
    requested: &str,
    opened: &str,
    opened_id: Option<&str>,
) -> Option<ObservedDevice> {
    Some(ObservedDevice {
        backend,
        requested: requested.to_string(),
        opened: opened.to_string(),
        opened_id: opened_id.map(str::to_string),
        // Une ouverture qui a abouti sur le nom demandé n'a aucun motif :
        // les motifs sont éprouvés à part, dans les tests de #3230.
        reason: None,
    })
}

// ------------------------------------------------------------------
// La famille, membre par membre — jamais un seul représentant.
// ------------------------------------------------------------------

/// Contre-épreuve PERMANENTE (leçon #1864) : chaque motif déclaré doit être
/// réellement PRODUIT par l'une des deux fonctions de décision. Un motif
/// ajouté à l'énumération sans être câblé dans `select_host` fait tomber ce
/// test — c'est exactement le défaut où 15 prédicats sur 17 n'étaient
/// jamais construits pendant leur propre test.
#[test]
fn chaque_motif_declare_est_reellement_produit() {
    let mut produits: Vec<LocalBackendFallback> = Vec::new();
    // Toutes les issues possibles du sondage ASIO.
    for probe in [None, Some(0usize), Some(1usize), Some(7usize)] {
        if let (_, Some(reason)) = asio_outcome(probe) {
            produits.push(reason);
        }
    }
    // Toutes les demandes possibles sur une cible sans ASIO.
    for requested in ["asio", "auto", "wasapi", "", "n'importe quoi"] {
        if let (_, Some(reason)) = unsupported_outcome(requested) {
            produits.push(reason);
        }
    }

    for motif in LocalBackendFallback::ALL {
        assert!(
            produits.contains(&motif),
            "le motif {motif:?} est déclaré mais AUCUN chemin de décision ne le construit — \
             il ne gardera jamais rien"
        );
    }
}

/// Les trois motifs doivent rester distincts, non vides, en `snake_case`,
/// et nommer ASIO : ce sont eux que le client reçoit et traduit.
#[test]
fn tous_les_motifs_ont_un_code_et_un_texte_utilisables() {
    let mut codes: Vec<&str> = Vec::new();
    for motif in LocalBackendFallback::ALL {
        let code = motif.code();
        assert!(!code.is_empty(), "{motif:?} : code vide");
        assert!(
            code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{motif:?} : code non snake_case ({code})"
        );
        assert!(
            code.starts_with("asio_"),
            "{motif:?} : le code doit nommer le backend demandé ({code})"
        );
        assert!(!codes.contains(&code), "code dupliqué : {code}");
        codes.push(code);

        let detail = motif.detail();
        assert!(!detail.is_empty(), "{motif:?} : détail vide");
        assert!(
            detail.contains("ASIO"),
            "{motif:?} : le détail ne dit pas ce qui a été demandé ({detail})"
        );
    }
    assert_eq!(codes.len(), LocalBackendFallback::ALL.len());
}

/// Le contrat JSON, pour la famille entière : `serde` doit rendre
/// exactement `code()`. Un renommage de variante casserait le client sans
/// ce test.
#[test]
fn la_serialisation_json_suit_le_code_pour_chaque_motif() {
    for motif in LocalBackendFallback::ALL {
        let json = serde_json::to_string(&motif).expect("sérialisation");
        assert_eq!(
            json,
            format!("\"{}\"", motif.code()),
            "{motif:?} : la charge utile ne porte pas son code stable"
        );
    }
}

// ------------------------------------------------------------------
// Les décisions, cas par cas.
// ------------------------------------------------------------------

/// LE cas Bilou (réponse forum 5217, 10/08, v0.9.65) : hôte ASIO ouvert,
/// zéro sortie exposée, repli WASAPI. Le journal le disait déjà
/// (`local_audio_asio_no_devices`) ; l'API se taisait.
#[test]
fn asio_ouvert_sans_peripherique_replie_en_nommant_la_cause() {
    assert_eq!(
        asio_outcome(Some(0)),
        ("WASAPI", Some(LocalBackendFallback::AsioNoDevices))
    );
}

/// L'autre membre : l'hôte ne s'ouvre pas du tout. Motif DIFFÉRENT — c'est
/// tout l'intérêt, Bertrand avait dû demander deux fois à Bilou laquelle
/// des deux lignes il voyait.
#[test]
fn hote_asio_inouvrable_donne_un_motif_distinct() {
    assert_eq!(
        asio_outcome(None),
        ("WASAPI", Some(LocalBackendFallback::AsioHostUnavailable))
    );
    assert_ne!(
        LocalBackendFallback::AsioHostUnavailable.code(),
        LocalBackendFallback::AsioNoDevices.code()
    );
}

/// Et le cas qui marche : aucun repli, aucun motif. On n'annonce pas une
/// panne quand il n'y en a pas.
#[test]
fn asio_qui_joue_ne_declare_aucun_repli() {
    assert_eq!(asio_outcome(Some(1)), ("ASIO", None));
    assert_eq!(asio_outcome(Some(9)), ("ASIO", None));
}

/// Le membre qui n'enregistrait RIEN avant ce correctif : un binaire sans
/// ASIO. Il ne pouvait pas honorer la demande, et ne le disait nulle part.
#[test]
fn binaire_sans_asio_nomme_la_cause() {
    assert_eq!(
        unsupported_outcome("asio"),
        (
            platform_default_backend_name(),
            Some(LocalBackendFallback::AsioUnsupportedBuild)
        )
    );
}

/// Contre-épreuve : sur la même cible, une demande qui n'est PAS ASIO ne
/// doit produire aucun motif. Plusieurs membres mutés, pas un seul.
#[test]
fn binaire_sans_asio_ne_crie_pas_sur_les_autres_demandes() {
    for requested in ["auto", "wasapi", "", "valeur inconnue"] {
        assert_eq!(
            unsupported_outcome(requested),
            (platform_default_backend_name(), None),
            "demande « {requested} » : motif inventé"
        );
    }
}

// ------------------------------------------------------------------
// L'arbitrage complet.
// ------------------------------------------------------------------

/// Le statut rendu à l'API dit les trois choses : ce qui tourne, ce qui
/// était demandé, et pourquoi ça diffère.
#[test]
fn le_statut_porte_lactif_le_demande_et_la_cause() {
    let s = backend_status(
        observed("WASAPI", Some(LocalBackendFallback::AsioNoDevices)),
        None,
        "ASIO",
    );
    assert_eq!(s.active, "WASAPI");
    assert_eq!(s.requested, "asio");
    assert!(s.fell_back);
    assert_eq!(s.fallback_reason, Some(LocalBackendFallback::AsioNoDevices));
    assert_eq!(
        s.fallback_detail,
        Some(LocalBackendFallback::AsioNoDevices.detail())
    );
}

/// Contre-épreuve : ASIO qui joue vraiment ne doit produire ni repli ni
/// motif. Le garde-fou doit savoir se taire.
#[test]
fn asio_honore_ne_declare_ni_repli_ni_motif() {
    let s = backend_status(observed("ASIO", None), None, "asio");
    assert_eq!(s.active, "ASIO");
    assert!(!s.fell_back, "repli annoncé alors qu'ASIO joue");
    assert_eq!(s.fallback_reason, None);
    assert_eq!(s.fallback_detail, None);
}

/// Contre-épreuve, sur plusieurs membres : les demandes honorées par le
/// backend natif de la plateforme ne déclarent rien non plus.
#[test]
fn les_demandes_honorees_ne_declarent_rien() {
    let natif = platform_default_backend_name();
    let natif_minuscules = natif.to_lowercase();
    for requested in ["auto", "", natif, natif_minuscules.as_str()] {
        let s = backend_status(observed(natif, None), None, requested);
        assert!(
            !s.fell_back,
            "demande « {requested} » sur {natif} : repli annoncé à tort"
        );
        assert_eq!(s.fallback_reason, None);
    }
}

/// Sans aucune observation, un seul motif est affirmable — celui qui se
/// décide à la COMPILATION. Sur une cible sans ASIO il doit sortir ; sur
/// une cible avec ASIO il ne doit surtout pas être inventé.
#[test]
fn sans_observation_seul_le_motif_de_compilation_est_affirme() {
    let s = backend_status(None, None, "asio");
    if asio_available() {
        assert_eq!(
            s.fallback_reason, None,
            "motif inventé sur une cible qui embarque ASIO"
        );
    } else {
        assert_eq!(
            s.fallback_reason,
            Some(LocalBackendFallback::AsioUnsupportedBuild)
        );
        assert!(s.fell_back);
    }
}

/// Une observation contredit toujours la déduction de compilation : si un
/// jour ASIO s'ouvre, plus aucun motif ne doit traîner.
#[test]
fn lobservation_prime_sur_la_deduction() {
    let s = backend_status(observed("ASIO", None), None, "asio");
    assert_eq!(s.fallback_reason, None);
    assert_eq!(s.active, "ASIO");
}

// ------------------------------------------------------------------
// Le PÉRIPHÉRIQUE — l'autre moitié de la vérité (#2207).
// ------------------------------------------------------------------

/// **Le fait de base.** Quand le backend ouvre un autre périphérique que
/// celui demandé, le statut porte LES DEUX noms et le dit.
///
/// C'est exactement la situation de #2207 : le chemin exclusif WASAPI
/// appelle `GetDefaultAudioEndpoint` quand la résolution par nom échoue,
/// donc une zone réglée sur un DAC joue sur les haut-parleurs. Le serveur
/// le savait déjà (`opened_device_name`), personne ne pouvait le lire.
#[test]
fn un_peripherique_different_du_demande_porte_les_deux_noms() {
    let s = backend_status(
        observed("WASAPI", None),
        ouvert(
            "WASAPI",
            "Topping D90 SE",
            "Haut-parleurs (Realtek Audio)",
            Some("{0.0.0.00000000}.{aaaa}"),
        ),
        "wasapi",
    );
    let d = s
        .device
        .expect("le statut doit porter le périphérique observé");
    assert_eq!(d.requested, "Topping D90 SE");
    assert_eq!(d.opened, "Haut-parleurs (Realtek Audio)");
    assert_eq!(d.backend, "WASAPI");
    assert_eq!(d.opened_id.as_deref(), Some("{0.0.0.00000000}.{aaaa}"));
    assert!(d.differs, "l'écart doit être signalé, c'est tout l'intérêt");
}

/// **Le témoin.** Le périphérique demandé est bien celui ouvert : aucun
/// écart ne doit être annoncé. Un garde-fou qui crie toujours ne sert à
/// rien — c'est la faute qu'ont déjà coûtée #2053 et #1315.
#[test]
fn un_peripherique_honore_nannonce_aucun_ecart() {
    let s = backend_status(
        observed("ALSA", None),
        ouvert(
            "ALSA",
            "Topping D90 SE",
            "Topping D90 SE",
            Some("hw:CARD=D90"),
        ),
        "auto",
    );
    let d = s.device.expect("périphérique observé");
    assert!(!d.differs, "écart annoncé alors que le DAC demandé joue");
}

/// Demander « default », c'est demander le périphérique système : le
/// recevoir n'est PAS un écart. Mais l'écran doit quand même pouvoir le
/// NOMMER — « default » ne dit rien à personne.
#[test]
fn default_demande_nest_pas_un_ecart_mais_reste_nomme() {
    let s = backend_status(
        observed("CoreAudio", None),
        ouvert("CoreAudio", "default", "MacBook Pro Speakers", None),
        "auto",
    );
    let d = s.device.expect("périphérique observé");
    assert!(!d.differs, "« default » honoré n'est pas un repli");
    assert_eq!(d.opened, "MacBook Pro Speakers");
    assert_eq!(
        d.opened_id, None,
        "CoreAudio n'expose aucun identifiant stable : le champ doit rester absent, pas inventé"
    );
}

/// **L'honnêteté de l'absence.** Rien n'a encore été ouvert : le champ est
/// absent, pas rempli d'une valeur plausible.
#[test]
fn sans_ouverture_observee_le_peripherique_est_absent() {
    let s = backend_status(observed("ALSA", None), None, "auto");
    assert!(
        s.device.is_none(),
        "un périphérique annoncé sans qu'aucun n'ait été ouvert est une invention"
    );
}

/// **Le faux ami.** `fell_back` parle du BACKEND (ASIO → WASAPI), `differs`
/// parle du PÉRIPHÉRIQUE. Les deux replis sont indépendants : le backend
/// demandé peut jouer et le DAC demandé être introuvable, et
/// réciproquement. Confondre les deux, c'est ré-annoncer #1395 à la place
/// de #2207.
#[test]
fn le_repli_de_backend_et_celui_de_peripherique_sont_independants() {
    // Backend honoré, périphérique dévié.
    let a = backend_status(
        observed("WASAPI", None),
        ouvert("WASAPI", "DAC USB", "Haut-parleurs", None),
        "wasapi",
    );
    assert!(!a.fell_back, "aucun repli de BACKEND ici");
    assert!(a.device.expect("périphérique").differs);

    // Backend dévié, périphérique honoré.
    let b = backend_status(
        observed("WASAPI", Some(LocalBackendFallback::AsioNoDevices)),
        ouvert("WASAPI", "DAC USB", "DAC USB", None),
        "asio",
    );
    assert!(b.fell_back, "repli de BACKEND attendu");
    assert!(!b.device.expect("périphérique").differs);
}

/// La charge utile JSON — ce que l'écran reçoit réellement — porte bien les
/// deux noms sous `device`. Un `assert` sur la structure Rust ne prouverait
/// rien du champ sérialisé.
#[test]
fn la_charge_utile_json_porte_les_deux_noms() {
    let s = backend_status(
        observed("WASAPI", None),
        ouvert("WASAPI", "Topping D90 SE", "Haut-parleurs", None),
        "wasapi",
    );
    let v = serde_json::to_value(&s).expect("statut sérialisable");
    assert_eq!(v["device"]["requested"], "Topping D90 SE");
    assert_eq!(v["device"]["opened"], "Haut-parleurs");
    assert_eq!(v["device"]["differs"], true);
    assert_eq!(v["device"]["backend"], "WASAPI");
    // Les champs de #1395 restent en place : cet ajout est additif.
    assert_eq!(v["active"], "WASAPI");
    assert_eq!(v["requested"], "wasapi");
}

/// **Le VERROU de branchement**, pour les chemins que Linux ne compile
/// pas : WASAPI exclusif, ASIO exclusif et CoreAudio exclusif vivent tous
/// trois sous un `#[cfg]` inatteignable depuis la machine de compilation.
/// Sans ce garde, on pourrait retirer un `note_opened_device` et tout
/// resterait vert — c'est LITTÉRALEMENT le défaut qu'on corrige : deux
/// accesseurs justes, un seul lecteur, une ligne de journal.
///
/// Même procédé que `chaque_sortie_de_select_host_enregistre_le_backend_ouvert`.
#[test]
fn chaque_chemin_douverture_enregistre_le_peripherique_ouvert() {
    let src = std::fs::read_to_string(std::path::Path::new("src/outputs/local.rs"))
        .expect("local.rs doit être lisible depuis la racine du crate");

    // 1. Les trois chemins EXCLUSIFS : chacun annonce sa lecture par une
    //    ligne `…_playing`, chacun doit enregistrer juste après.
    for (marqueur, backend) in [
        ("\"wasapi_exclusive_playing\"", "WASAPI"),
        ("\"local_audio_asio_exclusive_playing\"", "ASIO"),
        ("\"local_audio_exclusive_playing\"", "CoreAudio"),
    ] {
        let debut = src
            .find(marqueur)
            .unwrap_or_else(|| panic!("marqueur {marqueur} introuvable dans local.rs"));
        let fenetre = &src[debut..src.len().min(debut + 900)];
        assert!(
            fenetre.contains("note_opened_device(") && fenetre.contains(&format!("\"{backend}\"")),
            "le chemin {marqueur} joue sans dire QUEL périphérique il a ouvert \
             (attendu : un note_opened_device(\"{backend}\", …) juste après) : \
             la zone continuerait d'afficher la consigne au lieu de la vérité (#2207)"
        );
    }

    // 2. Le chemin cpal PARTAGÉ (ALSA, CoreAudio partagé, WASAPI partagé) :
    //    `find_device_with_fallback` a QUATRE sorties qui décident du son —
    //    « default » demandé, nom résolu, refus d'hôte étranger (#3230, la
    //    seule qui n'ouvre rien), repli sur le périphérique système
    //    (#2207). Les quatre doivent enregistrer : une sortie muette, c'est
    //    une zone qui affiche la consigne au lieu de la vérité.
    let debut = src
        .find("fn find_device_with_fallback(")
        .expect("find_device_with_fallback introuvable");
    let fin = src[debut..]
        .find("\n/// Probe a device's capabilities")
        .map(|i| debut + i)
        .expect("le corps de find_device_with_fallback doit précéder `Probe a device`");
    let corps = &src[debut..fin];
    let enregistrements = corps.matches("note_opened_device(").count()
        + corps.matches("note_device_outcome(").count();
    assert_eq!(
        enregistrements, 4,
        "find_device_with_fallback décide du son par quatre chemins mais n'en \
         enregistre que {enregistrements} : une sortie repart sans dire ce qu'elle a fait"
    );
    assert!(
        corps.contains("audio_device_not_found_falling_back_to_default"),
        "le repli sur le périphérique système a disparu — le cas à rendre visible n'existe plus"
    );
    assert!(
        corps.contains("audio_device_foreign_host_refused"),
        "le refus d'hôte étranger a disparu — un nom WASAPI redeviendrait \
         appariable à une sortie ASIO (#3230)"
    );
}

/// Le VERROU de branchement, pour la seule branche que PERSONNE ne peut
/// compiler ici.
///
/// La branche `#[cfg(all(target_os = "windows", feature = "asio"))]` de
/// `select_host` ne se compile qu'avec le SDK Steinberg et Visual Studio :
/// ni ce Mac, ni la machine de compilation Linux ne peuvent la toucher —
/// seul le job `windows-latest` de la CI y arrive. Les tests ci-dessus
/// éprouvent donc la DÉCISION (`asio_outcome`), pas son BRANCHEMENT. Sans
/// ce garde, on pourrait supprimer un `note_observed_backend` dans cette
/// branche et tout resterait vert sur trois plateformes sur quatre.
///
/// Même procédé que `contrat_des_retours_anticipes` côté serveur : on lit
/// la source, faute de pouvoir l'exécuter.
#[test]
fn chaque_sortie_de_select_host_enregistre_le_backend_ouvert() {
    let src = std::fs::read_to_string(std::path::Path::new("src/outputs/local.rs"))
        .expect("local.rs doit être lisible depuis la racine du crate");
    let debut = src
        .find("pub fn select_host(")
        .expect("select_host introuvable");
    let fin = src[debut..]
        .find("static OBSERVED_BACKEND")
        .map(|i| debut + i)
        .expect("le corps de select_host doit précéder OBSERVED_BACKEND");
    let corps = &src[debut..fin];

    // Une sortie = un host rendu. Chacune doit avoir dit LEQUEL avant de
    // le rendre, sinon l'API annonce de nouveau le backend demandé.
    let sorties =
        corps.matches("cpal::default_host()").count() + corps.matches("return host;").count();
    let enregistrements = corps.matches("note_observed_backend(").count();
    assert_eq!(
        enregistrements, sorties,
        "select_host rend {sorties} host(s) mais n'enregistre que {enregistrements} backend(s) : \
         un chemin repart sans dire ce qu'il a ouvert (c'est exactement le défaut de #1395)"
    );

    // Et la décision doit rester celle qu'on éprouve plus haut, pas une
    // règle réécrite en ligne dans la branche non compilable.
    for attendu in [
        "asio_outcome(Some(device_count))",
        "asio_outcome(None)",
        "unsupported_outcome(&backend_lower)",
    ] {
        assert!(
            corps.contains(attendu),
            "select_host ne passe plus par « {attendu} » — la décision testée n'est plus celle jouée"
        );
    }
}
