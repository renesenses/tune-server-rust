//! Témoin de la tranche T2 de #2373 (Tune Circle) : la réconciliation du
//! catalogue en ligne, exercée **à blanc**, n'émet **aucune écriture**.
//!
//! ## Pourquoi ce témoin, et pas seulement les gardes de la route
//!
//! `tune-server/src/routes/cloud.rs` porte deux gardes qui lisent leur propre
//! source : la route est montée, et son mode par défaut est `Mode::ABlanc`.
//! Elles retiennent ce que la route *demande*. Elles ne prouvent pas que le
//! mode à blanc n'écrit rien.
//!
//! Ici, c'est la **production qui agit** : `reconcilier()` est appelée telle
//! quelle, contre un cloud simulé et une base SQLite **en mémoire**, puis on
//! relit `sync_changelog`. Le témoin ne fabrique pas ce qu'il observe — il ne
//! pose aucun ordre de suppression lui-même ; il regarde si la production en
//! pose.
//!
//! Deux précautions contre un vert vide :
//!
//! 1. le rapport doit annoncer des orphelins **non nuls**. Sinon « zéro
//!    écriture » serait aussi vrai d'un code qui n'a rien fait du tout ;
//! 2. [`le_temoin_voit_les_ecritures_quand_la_reconciliation_applique`] compte
//!    **2** au même point d'observation quand la production écrit. Un zéro
//!    relevé sur une table que personne n'écrit jamais ne serait pas une
//!    preuve.
//!
//! ⛔ Aucun appel au cloud de production. `reconcilier()` reçoit son point
//! d'accès en paramètre ; il pointe ici sur un serveur local éphémère. La base
//! est `:memory:` — rien n'est écrit sur aucun disque.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tune_core::cloud::library_reconcile::{Mode, reconcilier};
use tune_core::db::backend::DbBackend;
use tune_core::db::{migrations, sqlite::SqliteDb};

const SERVEUR: &str = "serveur-temoin";
const JETON: &str = "jeton-de-test";

/// Ce que le cloud simulé porte. `1..=8` existent encore en local ; `99` et
/// `77` n'existent plus. Un orphelin sur neuf fait 11 %, sous le plafond de
/// 25 % au-delà duquel la production refuse d'agir : la garde de proportion
/// n'est donc pas ce qui empêche l'écriture ici.
const ARTISTES_EN_LIGNE: [i64; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 99];
const ALBUMS_EN_LIGNE: [i64; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 77];
const ORPHELIN_ARTISTE: i64 = 99;
const ORPHELIN_ALBUM: i64 = 77;

/// Base jetable, en mémoire, au schéma de production (`init_schema` puis les
/// migrations — c'est la 42 qui crée `sync_changelog`).
fn base(ensemencee: bool) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("sqlite en mémoire");
    db.init_schema().expect("schéma");
    migrations::run_migrations(&db).expect("migrations");
    if ensemencee {
        let mut sql = String::new();
        for id in 1..=8 {
            sql.push_str(&format!(
                "INSERT INTO artists (id, name) VALUES ({id}, 'Artiste {id}');\n\
                 INSERT INTO albums (id, title) VALUES ({id}, 'Album {id}');\n"
            ));
        }
        db.execute_batch(&sql).expect("semis");
    }
    Arc::new(db)
}

/// Relire le journal des changements — le seul endroit où la réconciliation
/// écrit.
///
/// `expect` et non un repli sur le vide : si la table n'existait pas, « zéro
/// ligne » serait un faux vert, et ce témoin passerait en gardant zéro chose.
fn ordres(backend: &Arc<dyn DbBackend>) -> Vec<(String, i64, String)> {
    backend
        .query_many(
            "SELECT entity_type, entity_id, action FROM sync_changelog ORDER BY entity_id",
            &[],
        )
        .expect("sync_changelog doit exister — sinon zéro ligne ne prouve rien")
        .iter()
        .map(|l| {
            (
                l[0].as_str().unwrap_or_default().to_string(),
                l[1].as_i64().unwrap_or_default(),
                l[2].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

struct CloudSimule {
    base_url: String,
    lectures: Arc<AtomicUsize>,
}

/// Un cloud local qui répond `statut` sur les deux inventaires.
fn page(ids: &[i64]) -> serde_json::Value {
    serde_json::json!({
        "data": ids
            .iter()
            .map(|i| serde_json::json!({ "remote_id": i }))
            .collect::<Vec<_>>(),
        "current_page": 1,
        "last_page": 1,
    })
}

async fn cloud_simule(statut: u16) -> CloudSimule {
    use axum::routing::get;

    let lectures = Arc::new(AtomicUsize::new(0));
    let code = axum::http::StatusCode::from_u16(statut).expect("statut");

    let pour_artistes = lectures.clone();
    let pour_albums = lectures.clone();

    let app = axum::Router::new()
        .route(
            &format!("/{SERVEUR}/artists"),
            get(move || {
                let compteur = pour_artistes.clone();
                async move {
                    compteur.fetch_add(1, Ordering::SeqCst);
                    (code, axum::Json(page(&ARTISTES_EN_LIGNE)))
                }
            }),
        )
        .route(
            &format!("/{SERVEUR}/albums"),
            get(move || {
                let compteur = pour_albums.clone();
                async move {
                    compteur.fetch_add(1, Ordering::SeqCst);
                    (code, axum::Json(page(&ALBUMS_EN_LIGNE)))
                }
            }),
        );

    // La socket écoute AVANT que la tâche ne démarre : pas de course, et donc
    // pas de `sleep` qui rendrait ce témoin intermittent sur un runner lent.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("écoute locale");
    let port = listener.local_addr().expect("adresse").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    CloudSimule {
        base_url: format!("http://127.0.0.1:{port}"),
        lectures,
    }
}

fn client() -> reqwest::Client {
    tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("client")
}

/// **La propriété.** À blanc, la réconciliation calcule le plan, le journalise,
/// et n'écrit rien.
#[tokio::test]
async fn la_reconciliation_a_blanc_n_emet_aucune_ecriture() {
    let backend = base(true);
    let cloud = cloud_simule(200).await;

    assert!(
        ordres(&backend).is_empty(),
        "le journal des changements doit partir vide"
    );

    let rapport = reconcilier(
        &backend,
        &client(),
        &cloud.base_url,
        SERVEUR,
        JETON,
        false,
        Mode::ABlanc,
    )
    .await;

    assert_eq!(
        rapport.refus, None,
        "aucune garde ne devait s'opposer ici : {rapport:?}"
    );
    assert!(
        rapport.a_blanc,
        "le rapport doit se dire à blanc : {rapport:?}"
    );
    assert_eq!(
        cloud.lectures.load(Ordering::SeqCst),
        2,
        "la production doit avoir LU les deux inventaires en ligne"
    );
    assert_eq!(
        (rapport.artistes_orphelins, rapport.albums_orphelins),
        (1, 1),
        "le plan doit voir la dérive ; sans elle, « zéro écriture » ne \
         prouverait rien : {rapport:?}"
    );

    let apres = ordres(&backend);
    assert!(
        apres.is_empty(),
        "à blanc, la réconciliation ne doit émettre AUCUNE écriture ; \
         trouvé : {apres:?}"
    );
}

/// Contre-épreuve interne : le même point d'observation compte bien les
/// écritures quand la production en fait. Sans ce témoin, le précédent serait
/// vert contre une table que personne n'écrit jamais.
///
/// Il retient aussi que la réconciliation ne touche **que** les orphelins :
/// aucun des huit ids encore locaux ne part à la suppression.
#[tokio::test]
async fn le_temoin_voit_les_ecritures_quand_la_reconciliation_applique() {
    let backend = base(true);
    let cloud = cloud_simule(200).await;

    let rapport = reconcilier(
        &backend,
        &client(),
        &cloud.base_url,
        SERVEUR,
        JETON,
        false,
        Mode::Appliquer,
    )
    .await;

    assert_eq!(rapport.refus, None, "{rapport:?}");
    assert!(!rapport.a_blanc, "{rapport:?}");

    let apres = ordres(&backend);
    assert_eq!(
        apres.len(),
        2,
        "en mode appliqué, les deux orphelins doivent être mis en file : {apres:?}"
    );
    assert!(
        apres.iter().all(|(_, _, action)| action == "delete"),
        "{apres:?}"
    );

    let ids: Vec<i64> = apres.iter().map(|(_, id, _)| *id).collect();
    assert!(ids.contains(&ORPHELIN_ARTISTE), "{apres:?}");
    assert!(ids.contains(&ORPHELIN_ALBUM), "{apres:?}");
    for vivant in 1..=8i64 {
        assert!(
            !ids.contains(&vivant),
            "l'id {vivant} existe encore en local : rien ne doit le supprimer \
             en ligne ; trouvé : {apres:?}"
        );
    }
}

/// Une erreur de **mesure** ne doit jamais devenir un verdict de suppression.
/// Le cloud répond 500 et la réconciliation est appelée en mode **appliqué** :
/// c'est le pire cas, et il ne doit produire aucune écriture.
#[tokio::test]
async fn un_cloud_qui_repond_mal_ne_fait_supprimer_personne() {
    let backend = base(true);
    let cloud = cloud_simule(500).await;

    let rapport = reconcilier(
        &backend,
        &client(),
        &cloud.base_url,
        SERVEUR,
        JETON,
        false,
        Mode::Appliquer,
    )
    .await;

    assert!(
        rapport.refus.is_some(),
        "un cloud en erreur doit produire un refus : {rapport:?}"
    );
    assert_eq!(
        (rapport.artistes_orphelins, rapport.albums_orphelins),
        (0, 0),
        "{rapport:?}"
    );
    let apres = ordres(&backend);
    assert!(
        apres.is_empty(),
        "une lecture en ligne ratée ne doit supprimer personne : {apres:?}"
    );
}

/// Le scénario qui viderait le cloud : une base locale non montée rend zéro
/// ligne. Éprouvé de bout en bout, mode **appliqué** compris — la garde est
/// dans `orphelins()`, ce témoin retient qu'elle est bien sur le chemin.
#[tokio::test]
async fn une_base_locale_vide_ne_fait_supprimer_personne() {
    let backend = base(false);
    let cloud = cloud_simule(200).await;

    let rapport = reconcilier(
        &backend,
        &client(),
        &cloud.base_url,
        SERVEUR,
        JETON,
        false,
        Mode::Appliquer,
    )
    .await;

    let refus = rapport.refus.clone().unwrap_or_default();
    assert!(
        refus.contains("lecture locale vide"),
        "une base vide doit être refusée, pas prise pour un catalogue effacé : \
         {rapport:?}"
    );
    let apres = ordres(&backend);
    assert!(apres.is_empty(), "{apres:?}");
}
