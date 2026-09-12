//! Les compteurs de bibliothèque disent DE QUOI ils sont faits (#2147).
//!
//! ## Ce que le testeur voyait
//!
//! Réglages → Bibliothèque affiche un compte de pistes, et une soixantaine de
//! lignes plus bas, dans la même section, le `total_files` du rapport de scan.
//! Chez lui : **142 pistes d'écart**, sans explication. Le corps de l'issue
//! soupçonnait une purge défaillante.
//!
//! Ce n'est pas une purge. Ce sont **deux populations différentes** :
//!
//! | Nombre | Ce qu'il compte |
//! |---|---|
//! | compteur de l'écran | les LIGNES de `tracks`, toutes sources confondues |
//! | `total_files` du scan | les FICHIERS TROUVÉS SUR LE DISQUE |
//!
//! Une piste Qobuz, Tidal, radio, podcast ou Bandcamp vit dans `tracks` sans
//! avoir le moindre fichier à trouver. Elle ne peut pas figurer dans un
//! rapport de scan, et pourtant elle est bien dans la bibliothèque. 142 pistes
//! non locales suffisent à produire l'écart ENTIER, sans qu'une seule ligne
//! ait été mal purgée.
//!
//! ## Ce que cette épreuve garde
//!
//! Elle mesure **le corps JSON des réponses**, jamais la condition SQL. Un
//! test qui rejouerait `GROUP BY source` ne garderait rien : il recopierait la
//! requête. Ici le banc est posé à la main — on SAIT qu'il contient cinq
//! pistes locales et sept non locales — et l'épreuve exige que les réponses le
//! disent.
//!
//! Le banc porte les **deux natures** délibérément. Sans piste non locale,
//! « le compte est bon » ne prouverait rien : le total et la part locale
//! seraient égaux et n'importe quelle requête passerait.
//!
//! ## Les deux routes, ensemble
//!
//! `/system/stats` est celle de l'écran du testeur ; `/library/stats` alimente
//! l'accueil, le tableau de bord et Oxygen. Elles affichent les MÊMES
//! compteurs. L'épreuve exige qu'elles ventilent pareil : deux écrans qui
//! divergent, c'est exactement le défaut qu'on referme.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::{Album, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

/// Le banc : combien de pistes par source. **Inventaire, pas échantillon.**
///
/// Les cinq sources non locales sont celles que le serveur écrit réellement
/// (`orchestrator.rs` : `qobuz`, `tidal`, `radio`, `podcast`, `bandcamp`).
const BANC_PISTES: &[(&str, usize)] = &[
    ("local", 5),
    ("qobuz", 3),
    ("tidal", 2),
    ("radio", 1),
    ("bandcamp", 1),
];

/// Le banc côté albums — la table porte la même colonne `source`.
const BANC_ALBUMS: &[(&str, usize)] = &[("local", 2), ("qobuz", 1)];

/// Total attendu de pistes : 5 + 3 + 2 + 1 + 1.
const PISTES_TOTAL: i64 = 12;
/// Part locale — celle que le rapport de scan peut retrouver sur le disque.
const PISTES_LOCALES: i64 = 5;
/// **L'écart de #2147, en miniature** : ce que le testeur ne s'expliquait pas.
const PISTES_NON_LOCALES: i64 = PISTES_TOTAL - PISTES_LOCALES;

const ALBUMS_TOTAL: i64 = 3;
const ALBUMS_LOCAUX: i64 = 2;

/// Plancher du détecteur : un banc vidé par mégarde doit ROUGIR, pas passer à
/// vide. Sans piste non locale l'épreuve ne prouverait plus rien (même patron
/// que `MINIMUM_DE_ROUTES` dans `pg_routes_serveur.rs`).
fn exige_un_banc_des_deux_natures() {
    assert_eq!(
        PISTES_TOTAL,
        BANC_PISTES.iter().map(|(_, n)| *n as i64).sum::<i64>(),
        "le banc et le total attendu ont divergé"
    );
    assert!(
        PISTES_NON_LOCALES > 0,
        "banc sans piste NON LOCALE : « le compte est bon » ne prouverait rien"
    );
    assert!(
        PISTES_LOCALES > 0,
        "banc sans piste LOCALE : la ventilation n'aurait rien à départager"
    );
    assert!(
        BANC_PISTES.len() >= 3,
        "le banc est retombé à {} sources : le détecteur perdrait sa portée",
        BANC_PISTES.len()
    );
}

/// Pose le banc dans la base de l'état fourni.
fn poser_le_banc(etat: &AppState) {
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    for (source, combien) in BANC_PISTES {
        for index in 0..*combien {
            let mut piste = Track::new(format!("{source} piste {index}"));
            piste.source = (*source).to_string();
            // Seul le local a un fichier sur le disque : c'est précisément ce
            // qui fait qu'un scan ne peut pas retrouver les autres.
            if *source == "local" {
                piste.file_path = Some(format!("/musique/{source}-{index}.flac"));
            }
            pistes
                .create(&piste)
                .unwrap_or_else(|erreur| panic!("piste {source} {index} : {erreur}"));
        }
    }

    let albums = AlbumRepo::with_backend(etat.backend.clone());
    for (source, combien) in BANC_ALBUMS {
        for index in 0..*combien {
            let mut album = Album::new(format!("{source} album {index}"));
            album.source = (*source).to_string();
            albums
                .create(&album)
                .unwrap_or_else(|erreur| panic!("album {source} {index} : {erreur}"));
        }
    }
}

/// La ventilation attendue, telle qu'elle doit APPARAÎTRE dans le corps JSON.
fn ventilation_attendue(banc: &[(&str, usize)]) -> Value {
    let mut objet = serde_json::Map::new();
    for (source, combien) in banc {
        objet.insert((*source).to_string(), json!(*combien as i64));
    }
    Value::Object(objet)
}

async fn corps_de(app: &Router, chemin: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap_or_else(|erreur| panic!("{chemin} : routeur en échec : {erreur}"));
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|erreur| panic!("{chemin} : corps illisible : {erreur}"));
    assert_eq!(
        statut,
        StatusCode::OK,
        "{chemin} : statut {statut}, corps {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets)
        .unwrap_or_else(|erreur| panic!("{chemin} : JSON illisible : {erreur}"))
}

/// Le cœur de l'épreuve : ce que le CORPS de la réponse doit dire.
fn le_corps_explique_l_ecart(chemin: &str, corps: &Value) {
    // 1. Le total ne bouge pas. La voie (b) a été retenue précisément pour
    //    cela : rien n'est retiré de l'écran. Si un jour quelqu'un filtre le
    //    compteur sur `source = 'local'`, c'est ICI que ça rougit.
    assert_eq!(
        corps["tracks"],
        json!(PISTES_TOTAL),
        "{chemin} : le compte de pistes doit rester le total de la bibliothèque, \
         toutes sources confondues — corps {corps}"
    );
    assert_eq!(
        corps["albums"],
        json!(ALBUMS_TOTAL),
        "{chemin} : le compte d'albums doit rester le total — corps {corps}"
    );

    // 2. La part locale est nommée : c'est le nombre que le rapport de scan
    //    montre, et le seul qui lui soit comparable.
    assert_eq!(
        corps["tracks_local"],
        json!(PISTES_LOCALES),
        "{chemin} : la part locale des pistes — corps {corps}"
    );
    assert_eq!(
        corps["albums_local"],
        json!(ALBUMS_LOCAUX),
        "{chemin} : la part locale des albums — corps {corps}"
    );

    // 3. La ventilation complète, source par source.
    assert_eq!(
        corps["tracks_by_source"],
        ventilation_attendue(BANC_PISTES),
        "{chemin} : ventilation des pistes — corps {corps}"
    );
    assert_eq!(
        corps["albums_by_source"],
        ventilation_attendue(BANC_ALBUMS),
        "{chemin} : ventilation des albums — corps {corps}"
    );

    // 4. L'INVARIANT qui rend la ventilation vérifiable : la somme des seaux
    //    égale le total. Une source oubliée par le `GROUP BY` — une ligne
    //    `NULL` non normalisée, par exemple — casse ici, et nulle part
    //    ailleurs.
    let somme: i64 = corps["tracks_by_source"]
        .as_object()
        .unwrap_or_else(|| panic!("{chemin} : tracks_by_source doit être un objet — {corps}"))
        .values()
        .map(|v| v.as_i64().unwrap_or(0))
        .sum();
    assert_eq!(
        somme, PISTES_TOTAL,
        "{chemin} : la somme des seaux ({somme}) doit égaler le total ({PISTES_TOTAL}) — \
         une source manque à l'appel, la ventilation mentirait par omission — corps {corps}"
    );

    // 5. **L'écart de #2147, rendu lisible par la réponse elle-même.** C'est
    //    la ligne qui répond à la question du testeur : ces pistes-là n'ont
    //    aucun fichier sur le disque, un scan ne peut pas les trouver.
    let ecart = corps["tracks"].as_i64().unwrap_or(0) - corps["tracks_local"].as_i64().unwrap_or(0);
    assert_eq!(
        ecart, PISTES_NON_LOCALES,
        "{chemin} : l'écart entre le compteur et ce qu'un scan peut retrouver \
         doit être exactement le nombre de pistes non locales — corps {corps}"
    );
}

/// Les deux routes affichent les mêmes compteurs sur deux écrans : elles
/// doivent les ventiler pareil, sinon on remplace un écart inexpliqué par un
/// autre.
fn les_deux_routes_s_accordent(gauche: &Value, droite: &Value) {
    for champ in [
        "tracks",
        "albums",
        "tracks_local",
        "albums_local",
        "tracks_by_source",
        "albums_by_source",
    ] {
        assert_eq!(
            gauche[champ], droite[champ],
            "/library/stats et /system/stats divergent sur `{champ}` : {} contre {}",
            gauche[champ], droite[champ]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn i2147_les_comptes_annoncent_leur_ventilation_par_source() {
    exige_un_banc_des_deux_natures();

    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    let bibliotheque = corps_de(&app, "/api/v1/library/stats").await;
    le_corps_explique_l_ecart("/library/stats", &bibliotheque);

    let systeme = corps_de(&app, "/api/v1/system/stats").await;
    le_corps_explique_l_ecart("/system/stats", &systeme);

    les_deux_routes_s_accordent(&bibliotheque, &systeme);
}

/// Les champs que le client web lit déjà doivent survivre intacts.
///
/// L'ajout est ADDITIF : `docs/contrat-web.json` exige `tracks`, `albums`,
/// `artists` de `/library/stats` et y ajoute `zones` et `devices` pour
/// `/system/stats`. Un champ SUPPRIMÉ planterait l'écran ; un champ ajouté
/// passe (`fetchJSON` fait un `as T` nu, sans validation). Cette épreuve tient
/// le premier risque à distance.
#[tokio::test(flavor = "multi_thread")]
async fn i2147_l_ajout_ne_retire_aucun_champ_existant() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    let bibliotheque = corps_de(&app, "/api/v1/library/stats").await;
    for champ in [
        "artists",
        "albums",
        "tracks",
        "listens",
        "zones",
        "total_duration_ms",
        "total_size_bytes",
    ] {
        assert!(
            bibliotheque.get(champ).is_some(),
            "/library/stats : champ `{champ}` disparu — corps {bibliotheque}"
        );
    }

    let systeme = corps_de(&app, "/api/v1/system/stats").await;
    for champ in [
        "artists",
        "albums",
        "tracks",
        "listens",
        "zones",
        "devices",
        "outputs",
        "server_version",
        "server_engine",
    ] {
        assert!(
            systeme.get(champ).is_some(),
            "/system/stats : champ `{champ}` disparu — corps {systeme}"
        );
    }
}

/// La même épreuve sur une VRAIE base PostgreSQL.
///
/// `COALESCE(NULLIF(source, ''), 'local')` répété dans le `GROUP BY` et
/// `ORDER BY 1` sont écrits pour être avalés par les deux moteurs. SQLite
/// avale à peu près tout ; PostgreSQL refuse. Sans cette épreuve, la
/// ventilation partirait vraie sur SQLite et en 500 chez tout utilisateur PG —
/// le motif exact de #2860 puis #2441.
///
/// ⚠️ Doctrine de garde, reprise de `pg_routes_serveur.rs` et **volontairement
/// différente de `pg_or_skip!`** (`tune-core/src/db/postgres_e2e.rs`), où une
/// connexion en échec rend `None` et affiche un test vert qui n'a rien
/// exécuté : ici, variable ABSENTE ⇒ saut annoncé ; variable POSÉE dont la
/// connexion échoue ⇒ le test TOMBE.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn i2147_pg_la_ventilation_par_source_tourne_sur_postgresql() {
    exige_un_banc_des_deux_natures();

    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #2147 sautée");
        return;
    };

    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    // Pas de `ok()?` : une connexion qui échoue doit ROUGIR, jamais sauter.
    let etat = AppState::new("", 0, config).expect("AppState sur PostgreSQL");

    // Les tables que l'épreuve compte, vidées AVANT : une étape précédente de
    // `test-postgres.yml` laisse des lignes, et un banc pollué ferait mentir
    // les totaux attendus.
    for table in ["tracks", "albums", "artists"] {
        etat.backend
            .execute(
                &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
                &[],
            )
            .unwrap_or_else(|erreur| panic!("vidage de {table} : {erreur}"));
    }

    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    let bibliotheque = corps_de(&app, "/api/v1/library/stats").await;
    le_corps_explique_l_ecart("/library/stats (PostgreSQL)", &bibliotheque);

    let systeme = corps_de(&app, "/api/v1/system/stats").await;
    le_corps_explique_l_ecart("/system/stats (PostgreSQL)", &systeme);

    les_deux_routes_s_accordent(&bibliotheque, &systeme);
}
