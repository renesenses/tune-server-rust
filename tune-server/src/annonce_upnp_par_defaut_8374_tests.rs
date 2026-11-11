//! #8374 — `TUNE_UPNP_SERVER_ENABLED` et `TUNE_UPNP_SERVER_NAME` étaient
//! déclarées dans `tune_core::config::TuneConfig` et lues par son `from_env`,
//! que le serveur n'appelle jamais : un utilisateur qui posait la variable
//! croyait couper l'annonce du serveur multimédia et ne coupait rien. Le vrai
//! réglage est la clé `upnp_enabled` (et `upnp_friendly_name`) de `settings`.
//!
//! Voie retenue : la variable d'environnement est la VALEUR PAR DÉFAUT quand
//! la clé est absente de la base ; la clé, posée depuis l'interface,
//! l'emporte toujours.
//!
//! Les épreuves passent la variable en paramètre (`*_avec`) : un
//! `std::env::set_var` dans un test casse la suite entière.
//!
//! Contre-épreuve (notée dans la PR) : rendre `annonce_voulue_selon` à son
//! ancien `unwrap_or(true)` (la branche `None => true`, sans lire la
//! variable) — la première épreuve rougit (mesuré sur Shrek) ; la deuxième
//! ne couvre que des cas où la clé est présente, elle garde la priorité de
//! la clé sur la variable. Retirer l'appel à `annonce_voulue` de
//! `spawn_upnp_advertiser` — la garde de câblage rougit.

use std::sync::Arc;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::upnp_server::{annonce_voulue_avec, nom_publie_avec};

fn base_vierge() -> SettingsRepo {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().expect("base en memoire");
    db.init_schema().expect("schema");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    SettingsRepo::with_backend(backend)
}

/// Le symptôme de l'issue : `TUNE_UPNP_SERVER_ENABLED=false` et aucune clé
/// en base — l'annonceur ne doit pas démarrer.
#[test]
fn la_variable_a_false_coupe_l_annonce_sans_cle_en_base() {
    let settings = base_vierge();
    assert!(
        !annonce_voulue_avec(&settings, Some("false")),
        "TUNE_UPNP_SERVER_ENABLED=false sans cle en base : l'annonce doit etre coupee"
    );
    assert!(
        !annonce_voulue_avec(&settings, Some("0")),
        "TUNE_UPNP_SERVER_ENABLED=0 sans cle en base : l'annonce doit etre coupee"
    );
    // Témoin : la variable à true, ou absente, laisse l'annonce active.
    assert!(annonce_voulue_avec(&settings, Some("true")));
    assert!(annonce_voulue_avec(&settings, Some("1")));
    assert!(
        annonce_voulue_avec(&settings, None),
        "sans variable ni cle : annonce active"
    );
}

/// La clé en base l'emporte, dans les deux sens : `upnp_enabled=true` posée
/// depuis l'interface démarre l'annonceur malgré la variable à false, et
/// `upnp_enabled=false` le coupe malgré la variable à true.
#[test]
fn la_cle_en_base_l_emporte_sur_la_variable() {
    let settings = base_vierge();
    settings.set("upnp_enabled", "true").unwrap();
    assert!(
        annonce_voulue_avec(&settings, Some("false")),
        "upnp_enabled=true en base doit l'emporter sur TUNE_UPNP_SERVER_ENABLED=false"
    );
    settings.set("upnp_enabled", "false").unwrap();
    assert!(
        !annonce_voulue_avec(&settings, Some("true")),
        "upnp_enabled=false en base doit l'emporter sur TUNE_UPNP_SERVER_ENABLED=true"
    );
    // Témoin : la clé seule, sans variable, garde sa sémantique historique.
    assert!(!annonce_voulue_avec(&settings, None));
    settings.set("upnp_enabled", "true").unwrap();
    assert!(annonce_voulue_avec(&settings, None));
}

/// Même règle pour le nom publié : clé `upnp_friendly_name` d'abord, puis
/// `TUNE_UPNP_SERVER_NAME`, puis `Tune (<nom>)` (#8189 — plus la constante
/// « Tune Server »). Un nom blanc ne compte pas.
#[test]
fn le_nom_suit_la_cle_puis_la_variable_puis_le_defaut() {
    let settings = base_vierge();
    settings.set("server_name", "Bureau").unwrap();
    assert_eq!(nom_publie_avec(&settings, None), "Tune (Bureau)");
    assert_eq!(nom_publie_avec(&settings, Some("")), "Tune (Bureau)");
    assert_eq!(nom_publie_avec(&settings, Some("   ")), "Tune (Bureau)");
    assert_eq!(
        nom_publie_avec(&settings, Some("Salon")),
        "Salon",
        "sans cle en base, TUNE_UPNP_SERVER_NAME donne le nom"
    );
    settings.set("upnp_friendly_name", "Cuisine").unwrap();
    assert_eq!(
        nom_publie_avec(&settings, Some("Salon")),
        "Cuisine",
        "la cle en base l'emporte sur TUNE_UPNP_SERVER_NAME"
    );
    settings.set("upnp_friendly_name", "  ").unwrap();
    assert_eq!(
        nom_publie_avec(&settings, Some("Salon")),
        "Salon",
        "une cle blanche en base ne compte pas"
    );
    assert_eq!(
        nom_publie_avec(&settings, None),
        "Tune (Bureau)",
        "ni cle ni variable : le defaut Tune (<server_name>)"
    );
}

/// #8189 — sans clé, sans variable et sans `server_name`, le défaut n'est
/// plus « Tune Server » mais `Tune (<nom de l'appareil>)`.
#[test]
fn le_defaut_sans_aucun_reglage_n_est_plus_tune_server() {
    let settings = base_vierge();
    let nom = nom_publie_avec(&settings, None);
    assert_ne!(nom, "Tune Server");
    assert!(nom.starts_with("Tune (") && nom.ends_with(')'), "{nom}");
}

/// Garde de câblage : la décision doit être prise par les fonctions
/// partagées, aux trois endroits qui la rendent — l'annonceur au démarrage,
/// l'écran de configuration (GET /api/v1/upnp/config) et le descriptif
/// (`UpnpState::new`). Sans cela, les épreuves ci-dessus prouvent une
/// fonction que personne n'appelle.
#[test]
fn l_annonceur_l_ecran_et_le_descriptif_passent_par_la_regle_partagee() {
    let fond = include_str!("background.rs");
    let debut = fond
        .find("async fn spawn_upnp_advertiser")
        .expect("spawn_upnp_advertiser a disparu de background.rs");
    let corps = &fond[debut..];
    let fin = corps.find("\n}\n").expect("fin de spawn_upnp_advertiser");
    let corps = &corps[..fin];
    assert!(
        corps.contains("tune_core::upnp_server::annonce_voulue(&settings)"),
        "spawn_upnp_advertiser ne passe plus par annonce_voulue : la variable \
         d'environnement n'est plus lue au demarrage"
    );
    assert!(
        !corps.contains(".unwrap_or(true)"),
        "spawn_upnp_advertiser a retrouve son ancien repli fixe a true"
    );

    let ecran = include_str!("routes/upnp.rs");
    assert!(
        ecran.contains("tune_core::upnp_server::annonce_voulue(&settings)")
            && ecran.contains("tune_core::upnp_server::nom_publie(&settings)"),
        "GET /api/v1/upnp/config ne reflete plus la regle partagee"
    );

    let descriptif = include_str!("../../tune-core/src/upnp_server.rs");
    let debut = descriptif
        .find("impl UpnpState {")
        .expect("impl UpnpState a disparu");
    let corps = &descriptif[debut..];
    let fin = corps.find("pub fn server_ip").expect("server_ip a disparu");
    assert!(
        corps[..fin].contains("let friendly_name = nom_publie(&settings);"),
        "UpnpState::new ne lit plus TUNE_UPNP_SERVER_NAME en repli"
    );
}
