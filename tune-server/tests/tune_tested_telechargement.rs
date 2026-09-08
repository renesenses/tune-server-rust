//! Le catalogue « Tune tested » quitte-t-il vraiment le réseau, et l'instance
//! se replie-t-elle vraiment hors ligne ? (#3589, volet A)
//!
//! # Aucun octet ne part vers mozaiklabs.fr
//!
//! Le nuage est **simulé dans le test** : un `axum::serve` sur
//! `127.0.0.1:0`, et `mozaik_base_url` qui l'y envoie — le même montage que
//! `licence_activation_immediate.rs` et `support_relais_diagnostic_sortant.rs`.
//! Un vrai serveur, et non une socket fermée à la main : un RST rend le test
//! instable.
//!
//! # Ce que ces cas verrouillent, et qui ne se voyait pas autrement
//!
//! Le comptage des requêtes est la seule preuve que « ne rien réappliquer si
//! la version n'a pas bougé » ne veut pas dire « ne plus jamais demander » : le
//! serveur redemande à chaque tour, et c'est la COMPARAISON qui décide. Un test
//! qui ne regarderait que la base rendrait vert sur une instance qui aurait
//! cessé d'interroger le site.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State as AxumState;
use axum::routing::get;
use tune_core::cloud::tune_tested::{self, Issue};
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

/// L'enveloppe réellement servie par mozaiklabs, recopiée du contrat de #3589.
///
/// 🔴 `gain_trim_db` y est un **entier**. C'est le piège que l'issue nomme, et
/// la raison d'être de ce corps de réponse littéral : le fabriquer depuis un
/// `f64` en Rust le ferait sortir en `-3.0` et ne garderait plus rien.
fn corps(version: i64) -> String {
    format!(
        r#"{{"version":{version},"generated_at":"2026-09-08T09:12:00+00:00","count":1,
            "settings_vocabulary":"tune.renderer.v1",
            "devices":[{{"brand":"Eversolo","model":"DMP-A8","output_type":"dlna",
              "settings":{{"dlna_native_flac":true,"gain_trim_db":-3}},
              "households":20,"validated_at":"2026-09-08T09:10:00+00:00",
              "note":"Le 192 sature sur firmware 1.4."}}]}}"#
    )
}

struct Simule {
    version: i64,
    appels: Arc<AtomicUsize>,
}

async fn nuage_simule(version: i64) -> (String, Arc<AtomicUsize>) {
    let appels = Arc::new(AtomicUsize::new(0));
    let etat = Arc::new(Simule {
        version,
        appels: appels.clone(),
    });
    let app = Router::new()
        .route(
            "/api/v1/community/devices/tune-tested",
            get(|AxumState(s): AxumState<Arc<Simule>>| async move {
                s.appels.fetch_add(1, Ordering::SeqCst);
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    corps(s.version),
                )
            }),
        )
        .with_state(etat);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let addr: SocketAddr = listener.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), appels)
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.init_schema().expect("schéma");
    migrations::run_migrations(&db).expect("migrations");
    Arc::new(db)
}

#[tokio::test]
async fn le_catalogue_se_telecharge_puis_ne_se_reapplique_pas() {
    let db = base();
    let settings = SettingsRepo::with_backend(db.clone());
    let (base_url, appels) = nuage_simule(1_788_800_000).await;
    settings.set("mozaik_base_url", &base_url).unwrap();

    assert_eq!(
        tune_tested::rafraichir(&db).await,
        Issue::Range {
            avant: 0,
            apres: 1_788_800_000
        }
    );
    assert_eq!(appels.load(Ordering::SeqCst), 1);

    // Le catalogue est là, et le trim ENTIER se relit en -3.0.
    let cat = tune_tested::catalogue_range(&settings).expect("catalogue rangé");
    let dev = cat
        .appareil("Eversolo", "DMP-A8", Some("dlna"))
        .expect("appareil validé");
    assert_eq!(dev.nombre("gain_trim_db"), Some(-3.0));
    assert_eq!(dev.drapeau("dlna_native_flac"), Some(true));

    // Second tour : le site sert la MÊME version. L'instance redemande — la
    // preuve est le compteur — mais ne réapplique rien.
    assert_eq!(
        tune_tested::rafraichir(&db).await,
        Issue::Inchange(1_788_800_000)
    );
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "l'instance doit continuer d'interroger le site, pas cesser de demander"
    );
}

/// 🔴 Hors ligne : rien ne bouge, et surtout rien ne s'efface.
#[tokio::test]
async fn hors_ligne_le_catalogue_en_place_est_conserve() {
    let db = base();
    let settings = SettingsRepo::with_backend(db.clone());

    // Un premier tour réussi, pour avoir quelque chose à perdre.
    let (base_url, _) = nuage_simule(1_788_800_000).await;
    settings.set("mozaik_base_url", &base_url).unwrap();
    assert!(matches!(
        tune_tested::rafraichir(&db).await,
        Issue::Range { .. }
    ));

    // Puis le réseau disparaît : une adresse qui n'écoute pas. Le port est
    // réservé puis relâché, donc personne n'y répond.
    let mort = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = mort.local_addr().unwrap();
    drop(mort);
    settings
        .set("mozaik_base_url", &format!("http://{addr}"))
        .unwrap();

    assert!(matches!(
        tune_tested::rafraichir(&db).await,
        Issue::Repli(_)
    ));
    // Ni la version ni le catalogue n'ont bougé : l'instance se comporte
    // exactement comme avant la panne.
    assert_eq!(tune_tested::version_connue(&settings), 1_788_800_000);
    let cat = tune_tested::catalogue_range(&settings).expect("catalogue conservé");
    assert!(cat.appareil("Eversolo", "DMP-A8", Some("dlna")).is_some());
}

/// Une instance qui n'a JAMAIS joint le site n'a pas de catalogue téléchargé —
/// et c'est bien ainsi : l'appelant se replie sur le catalogue embarqué.
#[tokio::test]
async fn une_instance_qui_n_a_jamais_joint_le_site_n_a_pas_de_catalogue() {
    let db = base();
    let settings = SettingsRepo::with_backend(db.clone());
    let mort = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = mort.local_addr().unwrap();
    drop(mort);
    settings
        .set("mozaik_base_url", &format!("http://{addr}"))
        .unwrap();

    assert!(matches!(
        tune_tested::rafraichir(&db).await,
        Issue::Repli(_)
    ));
    assert_eq!(tune_tested::version_connue(&settings), 0);
    assert!(tune_tested::catalogue_range(&settings).is_none());
    // Le catalogue embarqué, lui, est toujours là — le repli est réel.
    assert!(!tune_core::device_catalog::catalog().brands.is_empty());
}
