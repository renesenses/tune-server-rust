//! Les sections « Autres versions » et « Radios récentes » sur une VRAIE base
//! PostgreSQL, et le même résultat que sur SQLite (#3181).
//!
//! ## Ce qui a échappé aux portes
//!
//! Le journal de #3181 montre trois requêtes de l'accueil en échec chez un
//! testeur PostgreSQL, à la même seconde, dont deux traitées ici :
//!
//! - `for SELECT DISTINCT, ORDER BY expressions must appear in select list`
//!   — « Autres versions », qui triait sur `lh.listened_at` sans le
//!   sélectionner ;
//! - `operator does not exist: text = integer` — « Radios récemment
//!   écoutées », qui comparait `is_favorite` à l'entier `0` alors que la
//!   colonne est `TEXT` sur une base venue de SQLite.
//!
//! SQLite accepte les deux formes. Les deux sections partaient donc vides sur
//! PostgreSQL, et `ou_defaut_journalise` rendait `200 []` : rien à l'écran,
//! rien dans le corps de la réponse, une seule ligne dans le journal serveur.
//!
//! `pg_routes_serveur.rs` frappe déjà `/api/v1/home/recently-added` sur
//! PostgreSQL, mais il ne vérifie que le STATUT. Or ces deux routes rendent
//! `200` avec un corps vide quand leur requête échoue : un test de statut est
//! par construction aveugle à ce défaut. C'est pourquoi ce fichier sème des
//! données et exige des LIGNES.
//!
//! ## Les deux moteurs, dans le même test
//!
//! Réparer PostgreSQL en changeant ce que SQLite rend serait un échange, pas
//! une correction. La même semence et les mêmes requêtes HTTP passent donc par
//! un `AppState` PostgreSQL ET par un `AppState` SQLite, et les deux corps
//! sont comparés tels quels — même ensemble, même ORDRE. Les identifiants
//! auto-attribués sont neutralisés avant comparaison : ils viennent de deux
//! séquences distinctes et ne prouveraient rien.
//!
//! ⚠️ Doctrine du saut, reprise de `pg_routes_serveur.rs` : la variable
//! `TUNE_TEST_PG_URL` ABSENTE saute (le `cargo test` ordinaire n'a pas de
//! base), mais une variable POSÉE dont la connexion échoue fait TOMBER le
//! test. Un banc mal branché doit rougir, jamais s'afficher vert.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

/// Les routes sondées, et ce qu'on exige d'elles.
///
/// **Inventaire, pas échantillon** : le test refuse d'en voir moins que
/// [`MINIMUM_DE_ROUTES`], pour qu'une liste vidée par mégarde rougisse au lieu
/// de passer à vide.
const ROUTES_SONDEES: &[&str] = &[
    // #3181 — `SELECT DISTINCT` + `ORDER BY` hors liste de sélection.
    "/api/v1/home/other-versions",
    // #3181 — `is_favorite = 0` sur une colonne `TEXT`.
    "/api/v1/home/radio-picks",
];

/// Plancher du détecteur.
const MINIMUM_DE_ROUTES: usize = 2;

/// Les tables vidées avant la semence, dans l'ordre des dépendances.
///
/// `DELETE` et non `TRUNCATE` : la même instruction doit valoir sur les deux
/// moteurs, et SQLite ne connaît pas `TRUNCATE`. Le vidage n'est pas de la
/// propreté, c'est l'état sous test — les étapes `tune-core` qui précèdent
/// dans `test-postgres.yml` laissent des lignes derrière elles, et une table
/// non vide rendrait la comparaison entre moteurs dépendante de l'ordre des
/// étapes.
const TABLES_VIDEES: &[&str] = &[
    "listen_history",
    "tracks",
    "albums",
    "artists",
    "radio_stations",
];

/// L'URL du banc PostgreSQL, ou `None` quand la variable n'est pas posée.
fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

/// Monte l'état du serveur sur PostgreSQL — le chemin exact de la production,
/// `TuneConfig::database_url` en `postgres://…`. `AppState::new` applique
/// lui-même les migrations PG, comme au démarrage réel.
fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    // Pas de `ok()?` ici : une connexion qui échoue doit ROUGIR, jamais sauter.
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

/// Le même serveur sur SQLite en mémoire — la contre-épreuve.
fn etat_sqlite() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

/// Sème le scénario. Aucun paramètre lié : les marqueurs diffèrent d'un moteur
/// à l'autre (`?1` / `$1`), et une semence en SQL littéral vaut telle quelle
/// des deux côtés.
///
/// Les littéraux numériques sont QUOTÉS (`'296000'`, `'0'`). C'est la même
/// raison que la correction elle-même : ces colonnes sont `TEXT` sur une base
/// PostgreSQL venue de SQLite et numériques sur une installation neuve, et un
/// littéral quoté reste non typé à l'analyse — il se résout dans les deux cas,
/// là où un littéral nu ne vaudrait que pour l'un des deux schémas.
fn semer(state: &AppState) {
    for table in TABLES_VIDEES {
        state
            .backend
            .execute(&format!("DELETE FROM {table}"), &[])
            .unwrap_or_else(|e| panic!("vidage de {table} : {e}"));
    }

    let semence = [
        // ── « Autres versions » ──
        // Un morceau écouté depuis une compilation, possédé sur l'album de
        // l'artiste : c'est exactement ce que la section doit montrer.
        "INSERT INTO artists (name) VALUES ('Kate Bush')",
        "INSERT INTO albums (title, artist_id) \
         SELECT 'Before The Dawn', id FROM artists WHERE name = 'Kate Bush'",
        "INSERT INTO albums (title, artist_id) \
         SELECT 'The Kick Inside', id FROM artists WHERE name = 'Kate Bush'",
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Running Up That Hill', al.id, al.artist_id, '296000', '/i3181/dawn.flac' \
         FROM albums al WHERE al.title = 'Before The Dawn'",
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Running Up That Hill', al.id, al.artist_id, '298000', '/i3181/kick.flac' \
         FROM albums al WHERE al.title = 'The Kick Inside'",
        // TROIS écoutes du même morceau : les lignes que le `SELECT DISTINCT`
        // devait effondrer, et que sa réparation naïve aurait démultipliées.
        "INSERT INTO listen_history (title, artist_name, album_title, listened_at) \
         VALUES ('Running Up That Hill', 'Kate Bush', 'Hit Collection', '2026-08-28T09:32:00Z')",
        "INSERT INTO listen_history (title, artist_name, album_title, listened_at) \
         VALUES ('Running Up That Hill', 'Kate Bush', 'Hit Collection', '2026-08-29T18:04:00Z')",
        "INSERT INTO listen_history (title, artist_name, album_title, listened_at) \
         VALUES ('Running Up That Hill', 'Kate Bush', 'Hit Collection', '2026-08-30T07:11:00Z')",
        // ── « Radios » ──
        // Une favorite, une non-favorite datée, une jamais jouée.
        "INSERT INTO radio_stations (name, url, is_favorite, last_played) \
         VALUES ('i3181 Favorite', 'http://i3181/fav', '1', '2026-09-01T10:00:00Z')",
        "INSERT INTO radio_stations (name, url, is_favorite, last_played) \
         VALUES ('i3181 Recente', 'http://i3181/rec', '0', '2026-09-02T10:00:00Z')",
        "INSERT INTO radio_stations (name, url, is_favorite, last_played) \
         VALUES ('i3181 Jamais jouee', 'http://i3181/jamais', '0', NULL)",
    ];
    for sql in semence {
        state
            .backend
            .execute(sql, &[])
            .unwrap_or_else(|e| panic!("semence en echec : {sql}\n{e}"));
    }
}

/// Interroge une route et rend son corps JSON. Le statut est exigé 2xx : un
/// 404 ou un 401 prouverait que la requête SQL n'a jamais été atteinte.
async fn corps_de(state: &AppState, route: &str) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1024 * 1024)
        .await
        .expect("corps de reponse");
    assert!(
        statut.is_success(),
        "{route} → {statut} : {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets).expect("corps JSON")
}

/// Neutralise les identifiants auto-attribués, récursivement.
///
/// Les deux moteurs ont leurs propres séquences : comparer les identifiants
/// bruts ferait rougir le test pour une raison qui n'a rien à voir avec ce
/// qu'il garde. On garde en revanche la DISTINCTION nul / non nul, pour qu'une
/// colonne perdue en route ne se cache pas derrière la neutralisation.
fn sans_identifiants(valeur: &mut Value) {
    match valeur {
        Value::Object(map) => {
            for (cle, v) in map.iter_mut() {
                if cle == "id" || cle.ends_with("_id") {
                    if !v.is_null() {
                        *v = Value::String("<id>".into());
                    }
                } else {
                    sans_identifiants(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sans_identifiants),
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3181_sections_accueil_rendent_les_memes_lignes_que_sqlite() {
    assert!(
        ROUTES_SONDEES.len() >= MINIMUM_DE_ROUTES,
        "la liste des routes sondées est tombée à {} (< {MINIMUM_DE_ROUTES}) : \
         le détecteur passerait à vide",
        ROUTES_SONDEES.len()
    );

    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };

    let pg = etat_postgres(&url);
    semer(&pg);
    let sqlite = etat_sqlite();
    semer(&sqlite);

    for route in ROUTES_SONDEES {
        let mut corps_pg = corps_de(&pg, route).await;
        let mut corps_sqlite = corps_de(&sqlite, route).await;

        // 1. La requête rend des LIGNES sur PostgreSQL. C'est le cœur : avant
        //    #3181 elle échouait, `ou_defaut_journalise` rendait le défaut, et
        //    cette assertion est la seule qui distingue « rien à montrer » de
        //    « la requête est tombée ».
        let lignes_pg = corps_pg.as_array().expect("réponse en tableau");
        assert!(
            !lignes_pg.is_empty(),
            "{route} : PostgreSQL rend un tableau VIDE alors que la semence \
             garantit des lignes — la requête a échoué et sa panne a été avalée"
        );

        // 2. La contre-épreuve : le même ensemble, dans le même ordre.
        sans_identifiants(&mut corps_pg);
        sans_identifiants(&mut corps_sqlite);
        assert_eq!(
            corps_pg, corps_sqlite,
            "{route} : PostgreSQL et SQLite ne rendent pas la même chose"
        );
    }

    // 3. Ce que chaque section doit contenir, nommément. Sans cela, deux
    //    moteurs également faux passeraient la comparaison.
    let versions = corps_de(&pg, "/api/v1/home/other-versions").await;
    let groupes = versions.as_array().expect("groupes de versions");
    assert_eq!(groupes.len(), 1, "groupes rendus : {groupes:?}");
    let versions_du_groupe = groupes[0]["versions"]
        .as_array()
        .expect("versions du groupe");
    // Deux albums possédés portent le morceau, et trois écoutes ne les
    // proposent qu'une fois chacun : c'est le dédoublonnage que faisait le
    // `SELECT DISTINCT`, et que la correction devait garder.
    assert_eq!(
        versions_du_groupe.len(),
        2,
        "versions rendues : {versions_du_groupe:?}"
    );

    verifier_radios_recentes(&pg).await;

    // ── 4. Le schéma DU SIGNALEMENT : `is_favorite` en TEXT ──
    //
    // Sur une installation PostgreSQL NEUVE la colonne est `SMALLINT`
    // (`migrations/postgres/005_additional_tables.sql`), et `is_favorite = 0`
    // y aurait très bien marché. Ce n'est PAS la base de jfpaquet : son erreur
    // — `operator does not exist: text = integer` — ne peut venir que d'une
    // colonne `TEXT`, c'est-à-dire d'une base venue de SQLite, où
    // `pg_migrate.rs` déclare `is_favorite TEXT DEFAULT 0` et où aucune des
    // migrations numériques (010/011/013) ne la reconvertit — elles ne
    // reprennent, pour `radio_stations`, que `bitrate` et `play_count`.
    //
    // Une épreuve qui ne tournerait que sur le schéma neuf serait donc verte
    // contre le défaut signalé. On bascule la colonne dans le type du
    // signalement et on rejoue la même vérification.
    basculer_is_favorite(&pg, "TEXT");
    verifier_radios_recentes(&pg).await;
    // Rendue à son type d'origine : ce banc est partagé par les étapes
    // suivantes de `test-postgres.yml`.
    basculer_is_favorite(&pg, "SMALLINT");
}

/// Convertit `radio_stations.is_favorite` dans le type demandé.
///
/// `USING` explicite : PostgreSQL ne convertit ni `text`→`smallint` ni
/// l'inverse tout seul.
fn basculer_is_favorite(state: &AppState, type_cible: &str) {
    let sql = format!(
        "ALTER TABLE radio_stations \
         ALTER COLUMN is_favorite DROP DEFAULT, \
         ALTER COLUMN is_favorite TYPE {type_cible} USING is_favorite::{type_cible}, \
         ALTER COLUMN is_favorite SET DEFAULT 0"
    );
    state
        .backend
        .execute(&sql, &[])
        .unwrap_or_else(|e| panic!("bascule de is_favorite en {type_cible} : {e}"));
}

/// « Radios récemment écoutées » : la non-favorite datée, et elle seule.
async fn verifier_radios_recentes(state: &AppState) {
    let radios = corps_de(state, "/api/v1/home/radio-picks").await;
    let radios = radios.as_array().expect("radios");
    let recentes: Vec<&str> = radios
        .iter()
        .filter(|r| r["is_favorite"].as_bool() == Some(false))
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(
        recentes,
        vec!["i3181 Recente"],
        "radios récemment écoutées rendues : {recentes:?}"
    );
}
