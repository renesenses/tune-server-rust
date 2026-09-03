//! #3185 — la base de Tune ne doit plus dependre du repertoire de lancement.
//!
//! ## Ce qui se passait
//!
//! Jean Marie (FRIDER), macOS Ventura, fils forum 616 et 645, juin 2026 :
//! « je n'ai pas d'autres choix que de lancer Tune manuellement avec le
//! .command […] si je le relance manuellement je perds les zones ».
//!
//! `TuneConfig::load` resolvait un `db_path` relatif vers
//! `~/Library/Application Support/Tune` **seulement si aucune base ne vivait
//! deja dans le repertoire courant**. Le repli — le `else` — etait le defaut :
//!
//! * lance par le `.command` depuis le dossier d'installation, le serveur y
//!   trouvait un `tune.db` et l'ouvrait ;
//! * lance par le LaunchAgent, dont le repertoire courant est `/`, il n'en
//!   trouvait aucun et resolvait vers `Application Support`.
//!
//! Deux bases coexistaient, invisibles l'une a l'autre. Le bloc Windows, juste
//! au-dessus, prefixe lui INCONDITIONNELLEMENT par `%LOCALAPPDATA%` : Windows
//! n'a jamais eu cette ambiguite.
//!
//! ## Pourquoi ces tests peuvent exister
//!
//! Le bloc fautif vivait sous `#[cfg(target_os = "macos")]` : il n'est compile
//! sur AUCUNE machine de la CI hors macOS, et un test portant le meme `cfg`
//! serait vert contre rien. La regle a donc ete extraite dans
//! `plan_base_macos`, fonction **pure et sans `cfg`**, qui recoit ce qu'elle ne
//! peut pas savoir (HOME, repertoire courant, predicat d'existence) — le
//! patron de `tune_core::config::resolve_local_audio_backend`. Ses effets de
//! bord vivent dans `appliquer_plan_base_macos` et `copier_base_sqlite`, eux
//! aussi sans `cfg`. Ce fichier-ci s'execute donc sur Linux, Windows et macOS.
//!
//! Aucun test n'ecrit dans l'environnement du processus : `std::env::set_var`
//! est global et casserait la suite `--workspace`. Le HOME est un parametre.

use std::path::{Path, PathBuf};

use tune_core::test_scratch::{ScratchDir, scratch_dir_in};
use tune_server::config::{
    ActionBaseMacos, MACOS_DATA_SUBDIR, PlanBaseMacos, TuneConfig, appliquer_plan_base_macos,
    copier_base_sqlite, plan_base_macos,
};

/// Un bac a sable a soi, hors de `/tmp` (une fixture posee la se vide sous
/// certains hotes) et supprime tout seul en sortant de portee, panique
/// comprise — c'est `ScratchDir` qui s'en charge (#3030).
fn bac(etiquette: &str) -> ScratchDir {
    scratch_dir_in(env!("CARGO_TARGET_TMPDIR"), etiquette)
}

/// Le predicat d'existence reel : la regle est pure, mais on la nourrit du
/// vrai disque des que le test pose de vrais fichiers.
fn sur_le_disque(chemin: &Path) -> bool {
    chemin.exists()
}

/// Ecrit une base et ses annexes, avec un contenu reconnaissable.
fn poser_base(dossier: &Path, nom: &str, marque: &str) {
    std::fs::create_dir_all(dossier).expect("dossier de fixture");
    std::fs::write(dossier.join(nom), format!("base:{marque}")).expect("base");
    std::fs::write(
        dossier.join(format!("{nom}-wal")),
        format!("journal:{marque}"),
    )
    .expect("-wal");
    std::fs::write(
        dossier.join(format!("{nom}-shm")),
        format!("index:{marque}"),
    )
    .expect("-shm");
}

fn lire(chemin: &Path) -> String {
    std::fs::read_to_string(chemin).unwrap_or_else(|e| panic!("lecture {} : {e}", chemin.display()))
}

fn app_support(home: &Path) -> PathBuf {
    home.join(MACOS_DATA_SUBDIR)
}

fn config_nue() -> TuneConfig {
    TuneConfig {
        db_path: "tune.db".into(),
        artwork_dir: "artwork_cache".into(),
        ..Default::default()
    }
}

/// **1. L'ambiguite.** La MEME configuration, resolue depuis deux repertoires
/// courants differents, doit designer la MEME base.
///
/// Le premier repertoire porte un `tune.db` — c'est le dossier d'installation
/// d'ou part le `.command`. Le second n'en porte aucun — c'est le `/` du
/// LaunchAgent. Avant le correctif, le premier rendait `tune.db` (donc, une
/// fois ouvert, la base du dossier d'installation) et le second le chemin sous
/// `Application Support` : deux bases.
#[test]
fn deux_repertoires_de_lancement_designent_la_meme_base() {
    let racine = bac("i3185-ambiguite");
    let home = racine.join("home");
    let dossier_command = racine.join("Applications/Tune");
    let racine_launchagent = racine.join("racine");

    // Le dossier d'installation porte une base ; `/` n'en porte pas.
    poser_base(&dossier_command, "tune.db", "installation");
    std::fs::create_dir_all(&racine_launchagent).expect("racine");

    let depuis_command = plan_base_macos(
        "tune.db",
        "artwork_cache",
        home.to_str(),
        &dossier_command,
        sur_le_disque,
    );
    let depuis_launchagent = plan_base_macos(
        "tune.db",
        "artwork_cache",
        home.to_str(),
        &racine_launchagent,
        sur_le_disque,
    );

    assert_eq!(
        depuis_command.db_path, depuis_launchagent.db_path,
        "le chemin de base depend encore du repertoire de lancement — \
         c'est exactement le defaut du fil 616"
    );
    assert_eq!(
        depuis_command.db_path,
        app_support(&home).join("tune.db").to_string_lossy(),
        "le chemin retenu doit etre celui d'Application Support"
    );
    // `artwork_dir` suivait le meme chemin dans l'ancien bloc, et n'y etait
    // resolu que dans la branche « aucune base locale ».
    assert_eq!(
        depuis_command.artwork_dir, depuis_launchagent.artwork_dir,
        "le dossier de pochettes depend encore du repertoire de lancement"
    );
    // Et la base trouvee dans le dossier d'installation n'est pas oubliee :
    // elle est signalee comme a migrer.
    assert_eq!(
        depuis_command.action,
        ActionBaseMacos::Migrer {
            source: dossier_command.join("tune.db")
        }
    );
    assert_eq!(depuis_launchagent.action, ActionBaseMacos::Aucune);
}

/// **2. La migration.** Une base dans le repertoire de lancement, aucune sous
/// `Application Support` : apres demarrage elle est la, AVEC ses fichiers
/// annexes, et son contenu est intact.
///
/// Le `-wal` est la moitie invisible d'une base SQLite : le copier sans lui
/// perd les dernieres ecritures. Le patron des deux suffixes vient de
/// `tune_core::db_backup`.
#[test]
fn la_base_du_repertoire_de_lancement_est_migree_avec_ses_annexes() {
    let racine = bac("i3185-migration");
    let home = racine.join("home");
    let lancement = racine.join("Applications/Tune");
    poser_base(&lancement, "tune.db", "zones-de-jean-marie");

    let mut config = config_nue();
    let plan = plan_base_macos(
        &config.db_path,
        &config.artwork_dir,
        home.to_str(),
        &lancement,
        sur_le_disque,
    );
    appliquer_plan_base_macos(&mut config, plan);

    let cible = app_support(&home).join("tune.db");
    assert_eq!(config.db_path, cible.to_string_lossy());
    assert_eq!(
        lire(&cible),
        "base:zones-de-jean-marie",
        "la base migree n'a pas le contenu de l'originale"
    );
    for suffixe in ["-wal", "-shm"] {
        let annexe = app_support(&home).join(format!("tune.db{suffixe}"));
        assert!(
            annexe.exists(),
            "le fichier annexe {suffixe} n'a pas suivi la base — SQLite \
             rejouerait un journal etranger"
        );
    }
    assert_eq!(
        lire(&app_support(&home).join("tune.db-wal")),
        "journal:zones-de-jean-marie"
    );
    assert_eq!(
        lire(&app_support(&home).join("tune.db-shm")),
        "index:zones-de-jean-marie"
    );

    // La source n'est jamais detruite : c'est le filet de securite.
    assert!(
        lancement.join("tune.db").exists(),
        "la base d'origine a ete supprimee — une migration ne detruit rien"
    );
    assert_eq!(lire(&lancement.join("tune.db")), "base:zones-de-jean-marie");

    // Aucun temporaire de migration ne survit.
    let residus: Vec<String> = std::fs::read_dir(app_support(&home))
        .expect("lecture d'Application Support")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".migration-"))
        .collect();
    assert!(
        residus.is_empty(),
        "temporaires de migration laisses : {residus:?}"
    );

    // Le dossier de pochettes est resolu et cree.
    assert_eq!(
        config.artwork_dir,
        app_support(&home).join("artwork_cache").to_string_lossy()
    );
    assert!(Path::new(&config.artwork_dir).is_dir());
}

/// **3. Les deux bases.** La regle retenue : `Application Support` l'emporte —
/// c'est le seul chemin qui ne depende pas du lanceur — et **aucune des deux
/// n'est detruite**. La delaissee est nommee dans le journal.
#[test]
fn quand_les_deux_bases_existent_application_support_gagne_et_aucune_n_est_detruite() {
    let racine = bac("i3185-deux-bases");
    let home = racine.join("home");
    let lancement = racine.join("Applications/Tune");
    poser_base(&lancement, "tune.db", "celle-du-command");
    poser_base(&app_support(&home), "tune.db", "celle-du-launchagent");

    let mut config = config_nue();
    let plan = plan_base_macos(
        &config.db_path,
        &config.artwork_dir,
        home.to_str(),
        &lancement,
        sur_le_disque,
    );
    assert_eq!(
        plan.action,
        ActionBaseMacos::DeuxBases {
            delaissee: lancement.join("tune.db")
        }
    );
    appliquer_plan_base_macos(&mut config, plan);

    let cible = app_support(&home).join("tune.db");
    assert_eq!(config.db_path, cible.to_string_lossy());
    assert_eq!(
        lire(&cible),
        "base:celle-du-launchagent",
        "la base d'Application Support a ete ecrasee par celle du repertoire \
         de lancement"
    );
    for nom in ["tune.db", "tune.db-wal", "tune.db-shm"] {
        assert!(
            lancement.join(nom).exists(),
            "{nom} du repertoire de lancement a ete detruit"
        );
    }
    assert_eq!(lire(&lancement.join("tune.db")), "base:celle-du-command");
}

/// **4. Le temoin.** Une installation deja propre — base uniquement sous
/// `Application Support` — ne change pas de comportement : meme chemin, aucune
/// migration, et rien n'est cree dans le repertoire de lancement.
#[test]
fn une_installation_deja_propre_ne_change_pas_de_comportement() {
    let racine = bac("i3185-temoin");
    let home = racine.join("home");
    let lancement = racine.join("racine");
    std::fs::create_dir_all(&lancement).expect("racine");
    poser_base(&app_support(&home), "tune.db", "deja-propre");

    let mut config = config_nue();
    let plan = plan_base_macos(
        &config.db_path,
        &config.artwork_dir,
        home.to_str(),
        &lancement,
        sur_le_disque,
    );
    assert_eq!(plan.action, ActionBaseMacos::Aucune);
    appliquer_plan_base_macos(&mut config, plan);

    assert_eq!(
        config.db_path,
        app_support(&home).join("tune.db").to_string_lossy()
    );
    assert_eq!(lire(Path::new(&config.db_path)), "base:deja-propre");
    assert!(
        !lancement.join("tune.db").exists(),
        "une base a ete fabriquee dans le repertoire de lancement"
    );
    let contenu: Vec<String> = std::fs::read_dir(&lancement)
        .expect("lecture du repertoire de lancement")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        contenu.is_empty(),
        "le repertoire de lancement a ete sali : {contenu:?}"
    );
}

/// Lance DEPUIS `Application Support` : les deux chemins designent le meme
/// fichier. Rien a migrer, et surtout aucune « delaissee » a signaler.
#[test]
fn lance_depuis_application_support_la_base_n_est_pas_sa_propre_delaissee() {
    let racine = bac("i3185-meme-dossier");
    let home = racine.join("home");
    let dossier = app_support(&home);
    poser_base(&dossier, "tune.db", "sur-place");

    let plan = plan_base_macos(
        "tune.db",
        "artwork_cache",
        home.to_str(),
        &dossier,
        sur_le_disque,
    );
    assert_eq!(plan.action, ActionBaseMacos::Aucune);
    assert_eq!(plan.db_path, dossier.join("tune.db").to_string_lossy());
}

/// Un `db_path` absolu est honore tel quel : l'utilisateur a decide.
#[test]
fn un_chemin_absolu_est_honore_tel_quel() {
    let racine = bac("i3185-absolu");
    let home = racine.join("home");
    let absolu = racine.join("ailleurs/ma-base.db");
    let absolu = absolu.to_string_lossy().into_owned();

    let plan = plan_base_macos(
        &absolu,
        "artwork_cache",
        home.to_str(),
        &racine,
        sur_le_disque,
    );
    assert_eq!(plan.action, ActionBaseMacos::CheminAbsolu);
    assert_eq!(plan.db_path, absolu);

    let mut config = config_nue();
    config.db_path = absolu.clone();
    appliquer_plan_base_macos(&mut config, plan);
    assert_eq!(config.db_path, absolu);
    assert_eq!(config.artwork_dir, "artwork_cache");
}

/// Sans `HOME`, rien n'est resolu — on ne fabrique pas un chemin au hasard.
#[test]
fn sans_home_les_chemins_restent_inchanges() {
    let plan = plan_base_macos(
        "tune.db",
        "artwork_cache",
        None,
        Path::new("/quelque/part"),
        sur_le_disque,
    );
    assert_eq!(plan.action, ActionBaseMacos::HomeIntrouvable);
    assert_eq!(plan.db_path, "tune.db");
    assert_eq!(plan.artwork_dir, "artwork_cache");
    assert_eq!(plan.app_support, None);
}

/// **Un echec a mi-chemin ne laisse aucun etat partiel.** Ici l'annexe `-wal`
/// de la source est rendue illisible (c'est un DOSSIER, ce qui fait echouer
/// `fs::copy` — un substitut portable a n'importe quelle panne d'E/S). La
/// cible doit rester exactement dans l'etat ou elle etait, et la source
/// intacte.
#[test]
fn une_migration_interrompue_ne_laisse_rien_a_la_cible() {
    let racine = bac("i3185-echec");
    let source_dir = racine.join("source");
    let cible_dir = racine.join("cible");
    std::fs::create_dir_all(&cible_dir).expect("cible");
    std::fs::create_dir_all(&source_dir).expect("source");
    std::fs::write(source_dir.join("tune.db"), "base:intacte").expect("base");
    // L'annexe qui fera echouer la copie.
    std::fs::create_dir_all(source_dir.join("tune.db-wal")).expect("-wal en dossier");

    let cible = cible_dir.join("tune.db");
    let issue = copier_base_sqlite(&source_dir.join("tune.db"), &cible);
    assert!(issue.is_err(), "la copie aurait du echouer : {issue:?}");

    assert!(
        !cible.exists(),
        "une base partielle a ete laissee a la cible : SQLite l'ouvrirait"
    );
    let restes: Vec<String> = std::fs::read_dir(&cible_dir)
        .expect("lecture de la cible")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(restes.is_empty(), "residus a la cible : {restes:?}");
    assert_eq!(
        lire(&source_dir.join("tune.db")),
        "base:intacte",
        "la source a ete abimee par une migration ratee"
    );
}

/// Une base sans `-wal` ni `-shm` — le cas d'un arret propre — migre aussi.
#[test]
fn une_base_sans_annexes_migre_aussi() {
    let racine = bac("i3185-sans-annexes");
    let source_dir = racine.join("source");
    let cible_dir = racine.join("cible");
    std::fs::create_dir_all(&source_dir).expect("source");
    std::fs::create_dir_all(&cible_dir).expect("cible");
    std::fs::write(source_dir.join("tune.db"), "base:arret-propre").expect("base");

    let cible = cible_dir.join("tune.db");
    let octets = copier_base_sqlite(&source_dir.join("tune.db"), &cible).expect("migration");
    assert_eq!(octets, "base:arret-propre".len() as u64);
    assert_eq!(lire(&cible), "base:arret-propre");
    assert!(!cible_dir.join("tune.db-wal").exists());
    assert!(!cible_dir.join("tune.db-shm").exists());
}

/// Le plan est une VALEUR : on peut le construire a la main et verifier que
/// l'application n'invente rien de plus que ce qu'il dit.
#[test]
fn un_plan_sans_dossier_de_donnees_ne_touche_a_rien() {
    let mut config = config_nue();
    appliquer_plan_base_macos(
        &mut config,
        PlanBaseMacos {
            db_path: "/ailleurs/tune.db".into(),
            artwork_dir: "/ailleurs/artwork".into(),
            app_support: None,
            action: ActionBaseMacos::Aucune,
        },
    );
    assert_eq!(config.db_path, "tune.db");
    assert_eq!(config.artwork_dir, "artwork_cache");
}
