//! #3529 — recensement des chemins qui créent une zone.
//!
//! Le réglage « Créer automatiquement les zones » (`zone_auto_create`) existe
//! depuis longtemps, le client l'écrit bien, et **cinq** chemins de création
//! ne le consultaient pas — dont trois qui tournent seuls, sans aucun geste de
//! l'utilisateur (lot SSDP de démarrage, re-sondage des DLNA mémorisés,
//! sondeurs Squeezebox et HQPlayer). #1770 en avait corrigé un et recopié la
//! garde ; c'est la recopie qui a laissé passer les autres.
//!
//! Un essai de comportement ne peut pas voir un **sixième** site qui
//! n'appellerait pas la garde : il ne connaît que les chemins qu'on lui donne,
//! exactement comme le recensement de #1770. Ce fichier-ci compte donc les
//! sites dans la SOURCE, et échoue sur tout site nouveau.
//!
//! La garde elle-même est éprouvée par le comportement, dans
//! `tune-core/src/db/zone_repo.rs` (`auto_create_decoche_refuse_un_appareil_inconnu`
//! et ses voisins) : les deux essais sont complémentaires, aucun ne remplace
//! l'autre.

use std::path::{Path, PathBuf};

/// Tous les fichiers `.rs` sous `tune-server/src`.
fn sources() -> Vec<PathBuf> {
    fn descendre(dir: &Path, sortie: &mut Vec<PathBuf>) {
        let entrees = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for entree in entrees {
            let chemin = entree.expect("entrée de répertoire").path();
            if chemin.is_dir() {
                descendre(&chemin, sortie);
            } else if chemin.extension().is_some_and(|e| e == "rs") {
                sortie.push(chemin);
            }
        }
    }
    let racine = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sortie = Vec::new();
    descendre(&racine, &mut sortie);
    assert!(
        sortie.len() > 50,
        "le recensement doit voir toute l'arborescence, pas {} fichiers",
        sortie.len()
    );
    sortie.sort();
    sortie
}

fn relatif(chemin: &Path) -> String {
    let racine = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    chemin
        .strip_prefix(&racine)
        .unwrap_or(chemin)
        .to_string_lossy()
        .replace('\\', "/")
}

fn lire(relatif_au_manifeste: &str) -> String {
    let chemin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relatif_au_manifeste);
    std::fs::read_to_string(&chemin).unwrap_or_else(|e| panic!("{}: {e}", chemin.display()))
}

fn compte(foin: &str, aiguille: &str) -> usize {
    foin.matches(aiguille).count()
}

/// Appel NON gardé à `ZoneRepo::get_or_create`, tel qu'il s'écrit dans le code
/// de production. `get_or_create_si_autorise(` ne contient pas cette chaîne :
/// la parenthèse la termine.
const APPEL_NU: &str = "zone_repo.get_or_create(";

/// Les sites qui appellent encore `get_or_create` **sans** la garde intégrée,
/// avec le nombre d'appels attendu et la raison. Toute autre occurrence, dans
/// n'importe quel fichier de `tune-server/src`, fait échouer le recensement.
const SITES_NUS: &[(&str, &str, usize, &str)] = &[
    (
        "src/background.rs",
        APPEL_NU,
        1,
        "rescan hotplug local — garde propre `local_zone_action` (#1770) : la \
         sortie SYSTÈME par défaut a le droit de naître même réglage décoché",
    ),
    (
        "src/startup.rs",
        APPEL_NU,
        1,
        "audio local au démarrage — même garde `local_zone_action` que le rescan",
    ),
    (
        "src/discovery_setup.rs",
        APPEL_NU,
        3,
        "SSDP en direct, mDNS en direct, fournisseur — les trois lisent \
         `zone_auto_create_autorise()` juste avant, et rendent la main sur un refus",
    ),
    (
        "src/routes/devices.rs",
        APPEL_NU,
        1,
        "`ensure_zone` — appareil ajouté À LA MAIN (`POST /devices/add`) : le \
         geste de l'utilisateur vaut consentement, le réglage ne porte pas dessus",
    ),
    (
        "src/plugins.rs",
        "repo.get_or_create(&zone.name",
        1,
        "un greffon DÉCLARE ses zones : elles font partie de ce que \
         l'utilisateur a installé, ce n'est pas une découverte",
    ),
];

/// Les cinq chemins de #3529, chacun avec l'étiquette d'origine qu'il passe à
/// la garde. L'étiquette n'a qu'un usage — nommer le chemin dans le journal —
/// mais elle rend chaque conversion vérifiable une par une.
const CHEMINS_CORRIGES: &[(&str, &str, &str)] = &[
    (
        "src/background.rs",
        "\"ssdp_startup\"",
        "lot SSDP/DLNA au démarrage (`spawn_ssdp_startup_scan`) — tourne à chaque \
         démarrage du serveur",
    ),
    (
        "src/routes/devices.rs",
        "\"discovered_dlna\"",
        "re-sondage des DLNA mémorisés (`register_discovered_dlna`) — tourne à \
         chaque démarrage, avec réessais",
    ),
    (
        "src/routes/squeezebox.rs",
        "\"squeezebox_poller\"",
        "sondage Squeezebox (`discover_and_register`) — toutes les 60 s tant que \
         `squeezebox_enabled`",
    ),
    (
        "src/routes/hqplayer.rs",
        "\"hqplayer_discover\"",
        "sondage HQPlayer (`discover_and_register_inner`) — à chaque sondage tant \
         que `hqplayer_enabled`",
    ),
    (
        "src/routes/bridge.rs",
        "\"bridge_devices\"",
        "pont (`handle_devices`) — à chaque annonce d'appareils d'un pont",
    ),
];

/// Les cinq chemins que l'issue nomme passent tous par la garde unique, et
/// chacun se nomme dans le journal.
#[test]
fn les_cinq_chemins_signales_passent_par_la_garde() {
    for (fichier, origine, quoi) in CHEMINS_CORRIGES {
        let source = lire(fichier);
        assert!(
            source.contains("get_or_create_si_autorise("),
            "{fichier} ({quoi}) doit appeler `get_or_create_si_autorise`"
        );
        assert_eq!(
            compte(&source, origine),
            1,
            "{fichier} ({quoi}) doit passer l'étiquette d'origine {origine} \
             exactement une fois"
        );
        assert!(
            source.contains("CreationDeZone::Refusee"),
            "{fichier} ({quoi}) doit traiter explicitement le refus — sans ce \
             bras, un refus serait silencieux et indistinguable d'une création"
        );
    }
}

/// Le recensement lui-même : aucun appel nu ailleurs que dans l'inventaire,
/// et l'inventaire compte juste.
#[test]
fn aucun_site_de_creation_de_zone_hors_inventaire() {
    let connus: Vec<&str> = SITES_NUS.iter().map(|(f, _, _, _)| *f).collect();

    // 1. Le compte de chaque site connu est celui qu'on a inventorié. Un appel
    //    nu ajouté dans un fichier déjà listé casse ici.
    for (fichier, aiguille, attendu, pourquoi) in SITES_NUS {
        let source = lire(fichier);
        assert_eq!(
            compte(&source, aiguille),
            *attendu,
            "{fichier} : {attendu} appel(s) nu(s) attendu(s) à `{aiguille}` \
             ({pourquoi}). Un appel de plus ou de moins veut dire que \
             l'inventaire de #3529 n'est plus à jour : relire le chemin ajouté, \
             et le faire passer par `get_or_create_si_autorise` s'il se \
             déclenche tout seul."
        );
    }

    // 2. Aucun fichier hors inventaire n'appelle `get_or_create` sur un
    //    `zone_repo`. Un SIXIÈME chemin, dans un fichier neuf, casse ici — ce
    //    que #1770 ne pouvait pas voir.
    let mut intrus = Vec::new();
    for chemin in sources() {
        let relatif = relatif(&chemin);
        if connus.contains(&relatif.as_str()) {
            continue;
        }
        let source = std::fs::read_to_string(&chemin).expect("source lisible");
        for (numero, ligne) in source.lines().enumerate() {
            if ligne.contains(APPEL_NU) {
                intrus.push(format!("{relatif}:{} — {}", numero + 1, ligne.trim()));
            }
        }
    }
    assert!(
        intrus.is_empty(),
        "chemin(s) de création de zone hors inventaire — chacun doit soit \
         passer par `ZoneRepo::get_or_create_si_autorise`, soit être ajouté à \
         `SITES_NUS` avec la raison qui le dispense du réglage :\n{}",
        intrus.join("\n")
    );
}

/// La lecture du réglage ne vit plus qu'à UN endroit. C'est la recopie qui a
/// fait #3529 : cinq copies conformes, cinq chemins sans copie du tout.
#[test]
fn le_reglage_ne_se_relit_plus_a_la_main_dans_le_serveur() {
    let mut relectures = Vec::new();
    for chemin in sources() {
        let source = std::fs::read_to_string(&chemin).expect("source lisible");
        // `routes/system/config.rs` porte le DÉFAUT du réglage et sa route
        // d'écriture : c'est sa place, il n'est pas une garde de découverte.
        if relatif(&chemin) == "src/routes/system/config.rs" {
            continue;
        }
        for (numero, ligne) in source.lines().enumerate() {
            if ligne.contains("\"zone_auto_create\"") {
                relectures.push(format!(
                    "{}:{} — {}",
                    relatif(&chemin),
                    numero + 1,
                    ligne.trim()
                ));
            }
        }
    }
    assert!(
        relectures.is_empty(),
        "le réglage doit se lire par `ZoneRepo::zone_auto_create_autorise()`, \
         pas se relire à la main : c'est la recopie de cette lecture qui a \
         produit #3529.\n{}",
        relectures.join("\n")
    );
}

/// Les gardes qui restent écrites dans `tune-server` passent bien par le
/// lecteur unique.
#[test]
fn les_gardes_restantes_utilisent_le_lecteur_unique() {
    for fichier in [
        "src/background.rs",
        "src/startup.rs",
        "src/discovery_setup.rs",
    ] {
        let source = lire(fichier);
        assert!(
            source.contains("zone_auto_create_autorise()"),
            "{fichier} garde un appel nu à `get_or_create` : il doit lire le \
             réglage par `zone_auto_create_autorise()`"
        );
    }
    // Trois gardes dans `discovery_setup.rs`, une par chemin direct.
    assert_eq!(
        compte(
            &lire("src/discovery_setup.rs"),
            "zone_auto_create_autorise()"
        ),
        3,
        "SSDP direct, mDNS direct et fournisseur : trois gardes, trois lectures"
    );
}
