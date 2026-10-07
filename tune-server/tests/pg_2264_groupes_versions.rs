//! `GET /library/tracks/{id}/versions/groups` sur une VRAIE base PostgreSQL,
//! et la même réponse que sur SQLite (#2264).
//!
//! La route ajoute UNE requête : les pistes qui partagent l'ISRC (plié :
//! majuscules, sans tiret ni espace) ou le MBID d'enregistrement (minuscules)
//! de la référence. `UPPER`, `LOWER`, `TRIM`, `REPLACE` et `CAST(… AS TEXT)`
//! ont le même contrat sur les deux moteurs ; cette cible le PROUVE au lieu
//! de le supposer.
//!
//! Doctrine du saut, celle de `pg_2372_versions_par_piste.rs` :
//! `TUNE_TEST_PG_URL` absente saute ; posée et injoignable, elle ROUGIT.
//!
//! L'épreuve ignorée `mesure_requete_par_identifiant` chronomètre la route
//! sur 100 000 pistes, sur chacun des deux moteurs (chiffres dans la PR).

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

const TABLES_VIDEES: &[&str] = &["listen_history", "tracks", "albums", "artists"];

fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

/// Les épreuves de ce binaire montent chacune un `AppState` sur la MÊME base
/// PostgreSQL, en parallèle : deux démarrages simultanés y posent les mêmes
/// fonctions et se heurtent (« tuple concurrently updated »). Une épreuve à
/// la fois.
static UNE_A_LA_FOIS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn etat_sqlite() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

fn executer(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

/// Littéraux numériques QUOTÉS, comme dans `pg_2372` : ils se résolvent que
/// la colonne soit `TEXT` (base venue de SQLite) ou numérique.
fn semer(state: &AppState) {
    for table in TABLES_VIDEES {
        executer(state, &format!("DELETE FROM {table}"));
    }
    for sql in [
        "INSERT INTO artists (name) VALUES ('Michael Jackson')",
        "INSERT INTO albums (title, artist_id) SELECT 'Thriller', id FROM artists",
        "INSERT INTO albums (title, artist_id) SELECT 'Number Ones', id FROM artists",
        "INSERT INTO albums (title, artist_id) SELECT 'Singles', id FROM artists",
        "INSERT INTO albums (title, artist_id) SELECT 'Bad Tour', id FROM artists",
        // La référence, avec ISRC et MBID.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, format, isrc, \
         musicbrainz_recording_id, file_path) \
         SELECT 'Billie Jean', al.id, al.artist_id, '294000', 'flac', 'USSM18200001', \
                '0b1c-mbid', '/2264/ref.flac' FROM albums al WHERE al.title = 'Thriller'",
        // L'heuristique : même titre, 0,5 s.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, format, file_path) \
         SELECT 'Billie Jean', al.id, al.artist_id, '294500', 'mp3', '/2264/no.mp3' \
         FROM albums al WHERE al.title = 'Number Ones'",
        // ⭐ L'ISRC à tirets, sous un titre que le rapprochement par le titre
        // ne voit pas : SEULE la requête par identifiant le trouve.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, format, isrc, file_path) \
         SELECT 'Billie-Jean', al.id, al.artist_id, '280000', 'flac', 'us-sm1-82-00001', \
                '/2264/isrc.flac' FROM albums al WHERE al.title = 'Singles'",
        // ⭐ Le MBID en majuscules.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, format, \
         musicbrainz_recording_id, file_path) \
         SELECT 'BJ', al.id, al.artist_id, '250000', 'flac', ' 0B1C-MBID ', '/2264/mbid.flac' \
         FROM albums al WHERE al.title = 'Singles'",
        // Le live : un autre groupe.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, format, file_path) \
         SELECT 'Billie Jean (Live)', al.id, al.artist_id, '320000', 'flac', '/2264/live.flac' \
         FROM albums al WHERE al.title = 'Bad Tour'",
    ] {
        executer(state, sql);
    }
}

fn id_de(state: &AppState, chemin: &str) -> i64 {
    state
        .backend
        .query_one(
            &format!("SELECT id FROM tracks WHERE file_path = '{chemin}'"),
            &[],
        )
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or_else(|| panic!("aucune piste en {chemin}"))
}

async fn corps_de(state: &AppState, route: &str) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    assert!(
        statut.is_success(),
        "{route} → {statut} : {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets).expect("corps JSON")
}

fn sans_identifiants(valeur: &mut Value) {
    match valeur {
        Value::Object(map) => {
            for (cle, v) in map.iter_mut() {
                if (cle == "id" || cle.ends_with("_id")) && cle != "musicbrainz_recording_id" {
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

/// Les titres du groupe de la référence, triés.
fn titres_du_premier_groupe(corps: &Value) -> Vec<String> {
    let mut t: Vec<String> = corps["groups"][0]["members"]
        .as_array()
        .expect("members")
        .iter()
        .map(|m| m["title"].as_str().unwrap_or_default().to_string())
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_2264_les_groupes_sont_les_memes_que_sur_sqlite() {
    let _verrou = UNE_A_LA_FOIS.lock().await;
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let pg = etat_postgres(&url);
    semer(&pg);
    let sqlite = etat_sqlite();
    semer(&sqlite);

    let route = |s: &AppState| {
        format!(
            "/api/v1/library/tracks/{}/versions/groups?streaming=false&rule=local",
            id_de(s, "/2264/ref.flac")
        )
    };
    let mut corps_pg = corps_de(&pg, &route(&pg)).await;
    let mut corps_sqlite = corps_de(&sqlite, &route(&sqlite)).await;

    // La requête par identifiant a bien RENDU des lignes sur PostgreSQL : sa
    // panne serait avalée (`ou_defaut_journalise`) et passerait pour « pas
    // d'autre exemplaire ».
    assert_eq!(
        titres_du_premier_groupe(&corps_pg),
        vec!["BJ", "Billie Jean", "Billie Jean", "Billie-Jean"],
        "{corps_pg:#}"
    );
    assert_eq!(
        corps_pg["groups"].as_array().map(Vec::len),
        Some(2),
        "{corps_pg:#}"
    );

    sans_identifiants(&mut corps_pg);
    sans_identifiants(&mut corps_sqlite);
    assert_eq!(
        corps_pg, corps_sqlite,
        "PostgreSQL et SQLite ne rendent pas la même chose"
    );
}

/// Les index de la migration PG 086 (#2264) sont posés, sur les deux
/// moteurs, sous les noms que nomme la migration SQLite 122.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2264_les_index_des_identifiants_sont_poses() {
    let _verrou = UNE_A_LA_FOIS.lock().await;
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let pg = etat_postgres(&url);
    let noms: Vec<String> = pg
        .backend
        .query_many(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'tracks' \
             AND indexname IN ('idx_tracks_isrc_norm', 'idx_tracks_mbid_recording_norm') \
             ORDER BY indexname",
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect();
    assert_eq!(
        noms,
        vec!["idx_tracks_isrc_norm", "idx_tracks_mbid_recording_norm"]
    );
    // L'expression indexée est celle que la requête compare.
    let definition: String = pg
        .backend
        .query_one(
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'idx_tracks_isrc_norm'",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .unwrap();
    assert!(
        definition.contains("upper(replace(replace(isrc"),
        "{definition}"
    );
}

/// Médiane de 9 appels de la requête par identifiant (celle que la règle de
/// lecture et le regroupement font à chaque fois).
fn chrono_par_identifiant(state: &AppState) -> std::time::Duration {
    use tune_core::library::groupes_versions::Exemplaire;
    let reference = Exemplaire {
        track_id: Some(id_de(state, "/2264/ref.flac")),
        isrc: Some("USSM18200001".into()),
        mbid_enregistrement: Some("0b1c-mbid".into()),
        ..Default::default()
    };
    let mut durees = Vec::new();
    for _ in 0..9 {
        let t = std::time::Instant::now();
        let n = tune_core::library::versions_en_base::pistes_par_identifiant(
            &state.backend,
            &reference,
            reference.track_id,
            200,
        )
        .len();
        durees.push(t.elapsed());
        assert!(n >= 1);
    }
    durees.sort();
    durees[4]
}

/// Chronomètre la route sur 100 000 pistes sans rapport, sur chaque moteur.
/// `cargo test … -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "mesure, pas une épreuve"]
async fn mesure_requete_par_identifiant() {
    let _verrou = UNE_A_LA_FOIS.lock().await;
    const N: i64 = 100_000;
    let mut etats = vec![("SQLite", etat_sqlite())];
    if let Some(url) = url_pg() {
        etats.push(("PostgreSQL", etat_postgres(&url)));
    }
    for (nom, etat) in etats {
        semer(&etat);
        let remplissage = match nom {
            "SQLite" => format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {N}) \
                 INSERT INTO tracks (title, album_id, artist_id, duration_ms, isrc, file_path) \
                 SELECT 'Morceau ' || i, (SELECT id FROM albums WHERE title = 'Singles'), \
                        (SELECT id FROM artists), 200000, 'FRZ12' || i, '/bruit/' || i FROM n"
            ),
            _ => format!(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, isrc, file_path) \
                 SELECT 'Morceau ' || i, (SELECT id FROM albums WHERE title = 'Singles'), \
                        (SELECT id FROM artists), 200000, 'FRZ12' || i, '/bruit/' || i \
                 FROM generate_series(1, {N}) AS i"
            ),
        };
        executer(&etat, &remplissage);
        let route = format!(
            "/api/v1/library/tracks/{}/versions/groups?streaming=false",
            id_de(&etat, "/2264/ref.flac")
        );
        let historique = format!(
            "/api/v1/library/tracks/{}/versions?streaming=false",
            id_de(&etat, "/2264/ref.flac")
        );
        // Une passe à vide, puis la médiane de 9.
        corps_de(&etat, &route).await;
        for (libelle, r) in [("versions", &historique), ("versions/groups", &route)] {
            let mut durees = Vec::new();
            for _ in 0..9 {
                let t = std::time::Instant::now();
                corps_de(&etat, r).await;
                durees.push(t.elapsed());
            }
            durees.sort();
            eprintln!("{nom} — {N} pistes — {libelle} : médiane {:?}", durees[4]);
        }
        // #2264 — le gain des index de la migration 122 / PG 086 : la même
        // requête, avec puis sans eux.
        if nom == "PostgreSQL" {
            executer(&etat, "ANALYZE tracks");
        }
        let avec = chrono_par_identifiant(&etat);
        executer(&etat, "DROP INDEX idx_tracks_isrc_norm");
        executer(&etat, "DROP INDEX idx_tracks_mbid_recording_norm");
        if nom == "PostgreSQL" {
            executer(&etat, "ANALYZE tracks");
        }
        let sans = chrono_par_identifiant(&etat);
        // Remis en place : la base de test PG est partagée entre épreuves.
        for ordre in tune_core::db::migrations::SQL_INDEX_IDENTIFIANTS_D_ENREGISTREMENT
            .split(';')
            .map(str::trim)
            .filter(|o| !o.is_empty())
        {
            executer(&etat, ordre);
        }
        eprintln!(
            "{nom} — {N} pistes — requête par identifiant : avec index {avec:?}, sans index {sans:?}"
        );
    }
}
