//! `GET /library/tracks/{id}/versions` sur une VRAIE base PostgreSQL, et le
//! même ensemble dans le même ORDRE que sur SQLite (#2372).
//!
//! ## Pourquoi cette cible existe
//!
//! `pg_3181_sections_accueil.rs` couvre `GET /home/other-versions`. Ce n'est
//! **pas** la même route : sa SQL est distincte, elle part de
//! `listen_history`, et elle n'a jamais partagé le `SELECT DISTINCT` qui a
//! fait tomber #3181. La route par piste (`routes/library/tracks.rs`, prédicat
//! partagé dans `routes/versions.rs`) n'était donc gardée sur PostgreSQL par
//! rien du tout.
//!
//! ## Ce que ce fichier garde, et que rien d'autre ne garde
//!
//! Le lot #2372 déplace le TRI de la requête vers Rust. Ce n'est pas un
//! raffinement : `ORDER BY al.title` compare sous la collation du moteur —
//! binaire pour SQLite, locale pour PostgreSQL. La semence ci-dessous contient
//! exactement les titres d'album sur lesquels les deux collations divergent :
//!
//! - `the wall` (minuscule) et `Thriller` : en octets, `T` (0x54) précède `t`
//!   (0x74), donc SQLite range `Thriller` d'abord ; sous une collation locale
//!   (`en_US.UTF-8`, `fr_FR.UTF-8`, ICU…) la casse est ignorée au premier
//!   passage et c'est `the wall` qui précède `Thriller`.
//! - `Éclipse` et `Zenith` : en octets `É` (0xC3 0x89) suit `Z` (0x5A), donc
//!   SQLite range `Zenith` d'abord ; une collation locale traite `É` comme `E`
//!   et range `Éclipse` d'abord.
//!
//! Un tri laissé au moteur rendrait donc deux ORDRES différents sur les deux
//! bases — et, sous pagination, des lignes qui réapparaissent d'une page à
//! l'autre. Le tri Rust par score puis par clé minusculée ne dépend, lui, que
//! des octets : c'est ce que ce fichier mesure.
//!
//! ## Doctrine du saut
//!
//! Reprise mot pour mot de `pg_routes_serveur.rs` et de
//! `pg_3181_sections_accueil.rs` : la variable `TUNE_TEST_PG_URL` **absente**
//! saute (le `cargo test` ordinaire n'a pas de base), mais une variable
//! **posée** dont la connexion échoue fait TOMBER le test. Un banc mal branché
//! doit rougir, jamais s'afficher vert. `pg_or_skip!` rendrait `None` dans les
//! deux cas ; c'est précisément ce qu'on n'utilise pas ici.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

/// Les tables vidées avant la semence, dans l'ordre des dépendances.
///
/// `DELETE` et non `TRUNCATE` : la même instruction doit valoir sur les deux
/// moteurs, et SQLite ne connaît pas `TRUNCATE`.
const TABLES_VIDEES: &[&str] = &["listen_history", "tracks", "albums", "artists"];

/// Les albums semés, dans l'ordre où ils sont insérés.
///
/// **Inventaire, pas échantillon** : chacun est là pour une raison écrite en
/// en-tête de fichier. Le test refuse d'en voir moins que [`MINIMUM_D_ALBUMS`],
/// pour qu'une semence vidée par mégarde rougisse au lieu de passer à vide.
const ALBUMS_DIVERGENTS: &[&str] = &["Thriller", "the wall", "Zenith", "Éclipse"];

/// Plancher du détecteur.
const MINIMUM_D_ALBUMS: usize = 4;

/// L'URL du banc PostgreSQL, ou `None` quand la variable n'est pas posée.
fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

/// Monte l'état du serveur sur PostgreSQL — le chemin exact de la production.
/// Pas de `ok()?` : une connexion qui échoue doit ROUGIR, jamais sauter.
fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
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
/// Les littéraux numériques sont QUOTÉS (`'291000'`). Ces colonnes sont `TEXT`
/// sur une base PostgreSQL venue de SQLite et numériques sur une installation
/// neuve ; un littéral quoté reste non typé à l'analyse et se résout dans les
/// deux cas.
fn semer(state: &AppState) {
    for table in TABLES_VIDEES {
        state
            .backend
            .execute(&format!("DELETE FROM {table}"), &[])
            .unwrap_or_else(|e| panic!("vidage de {table} : {e}"));
    }

    let semence = [
        "INSERT INTO artists (name) VALUES ('Sade')",
        // L'homonyme : même titre, autre artiste. Il ne doit RIEN rendre.
        "INSERT INTO artists (name) VALUES ('The Smooth Operators')",
        // Les quatre albums de l'artiste cherché, choisis pour que la
        // collation binaire et la collation locale les rangent DIFFÉREMMENT.
        "INSERT INTO albums (title, artist_id) \
         SELECT 'Thriller', id FROM artists WHERE name = 'Sade'",
        "INSERT INTO albums (title, artist_id) \
         SELECT 'the wall', id FROM artists WHERE name = 'Sade'",
        "INSERT INTO albums (title, artist_id) \
         SELECT 'Zenith', id FROM artists WHERE name = 'Sade'",
        "INSERT INTO albums (title, artist_id) \
         SELECT 'Éclipse', id FROM artists WHERE name = 'Sade'",
        // L'album de départ, et celui de l'homonyme.
        "INSERT INTO albums (title, artist_id) \
         SELECT 'Diamond Life', id FROM artists WHERE name = 'Sade'",
        "INSERT INTO albums (title, artist_id) \
         SELECT 'Ailleurs', id FROM artists WHERE name = 'The Smooth Operators'",
        // La piste de départ.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, isrc, file_path) \
         SELECT 'Smooth Operator', al.id, al.artist_id, '291000', 'GBAAA8400001', \
                '/i2372/diamond.flac' \
         FROM albums al WHERE al.title = 'Diamond Life'",
        // Quatre autres versions, TOUTES au même score : même titre exact,
        // même durée, aucun ISRC. Rien d'autre que la clé de départage ne peut
        // donc décider de leur ordre — c'est exactement le cas où un tri laissé
        // au moteur diverge.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Smooth Operator', al.id, al.artist_id, '291000', '/i2372/thriller.flac' \
         FROM albums al WHERE al.title = 'Thriller'",
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Smooth Operator', al.id, al.artist_id, '291000', '/i2372/wall.flac' \
         FROM albums al WHERE al.title = 'the wall'",
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Smooth Operator', al.id, al.artist_id, '291000', '/i2372/zenith.flac' \
         FROM albums al WHERE al.title = 'Zenith'",
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Smooth Operator', al.id, al.artist_id, '291000', '/i2372/eclipse.flac' \
         FROM albums al WHERE al.title = 'Éclipse'",
        // ⭐ Le cas de Gros Bidon : le remaster nommé AVEC UN TIRET, sur un
        // album de l'artiste. Il partage l'ISRC de la référence, donc il doit
        // sortir EN TÊTE quelle que soit la collation.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, isrc, file_path) \
         SELECT 'Smooth Operator - 2011 Remastered', al.id, al.artist_id, '291000', \
                'GBAAA8400001', '/i2372/remaster.flac' \
         FROM albums al WHERE al.title = 'Zenith'",
        // ⭐ L'homonyme d'un AUTRE artiste, remasterisé lui aussi. Le
        // rapprochement nomme l'artiste : il ne doit jamais sortir.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Smooth Operator - 2011 Remastered', al.id, al.artist_id, '180000', \
                '/i2372/homonyme.flac' \
         FROM albums al WHERE al.title = 'Ailleurs'",
        // ⭐ LE TÉMOIN : un morceau sans aucune autre version.
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         SELECT 'Your Love Is King', al.id, al.artist_id, '208000', '/i2372/temoin.flac' \
         FROM albums al WHERE al.title = 'Diamond Life'",
    ];
    for sql in semence {
        state
            .backend
            .execute(sql, &[])
            .unwrap_or_else(|e| panic!("semence en echec : {sql}\n{e}"));
    }
}

/// L'identifiant d'une piste par son chemin — les séquences des deux moteurs
/// sont distinctes, on ne peut pas coder l'id en dur.
fn id_de(state: &AppState, chemin: &str) -> i64 {
    state
        .backend
        .query_one(
            &format!("SELECT id FROM tracks WHERE file_path = '{chemin}'"),
            &[],
        )
        .unwrap_or_else(|e| panic!("lecture de {chemin} : {e}"))
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or_else(|| panic!("aucune piste en {chemin}"))
}

/// Interroge la route et rend son corps JSON. Le statut est exigé 2xx : un 404
/// prouverait que la requête SQL n'a jamais été atteinte.
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

/// Neutralise les identifiants auto-attribués, récursivement. Les deux moteurs
/// ont leurs propres séquences. La DISTINCTION nul / non nul est conservée,
/// pour qu'une colonne perdue en route ne se cache pas derrière.
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

/// Les titres d'album rendus, dans l'ordre.
fn albums_rendus(corps: &Value) -> Vec<String> {
    corps["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(|v| v["album_title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_2372_versions_par_piste_rendent_le_meme_ordre_que_sqlite() {
    assert!(
        ALBUMS_DIVERGENTS.len() >= MINIMUM_D_ALBUMS,
        "la semence divergente est tombée à {} albums (< {MINIMUM_D_ALBUMS}) : \
         le détecteur passerait à vide",
        ALBUMS_DIVERGENTS.len()
    );

    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };

    let pg = etat_postgres(&url);
    semer(&pg);
    let sqlite = etat_sqlite();
    semer(&sqlite);

    // `streaming=false` : aucun service n'est authentifié sur un banc de test,
    // et cette épreuve porte sur la requête locale et son ORDRE.
    let route_pg = format!(
        "/api/v1/library/tracks/{}/versions?streaming=false",
        id_de(&pg, "/i2372/diamond.flac")
    );
    let route_sqlite = format!(
        "/api/v1/library/tracks/{}/versions?streaming=false",
        id_de(&sqlite, "/i2372/diamond.flac")
    );

    let mut corps_pg = corps_de(&pg, &route_pg).await;
    let mut corps_sqlite = corps_de(&sqlite, &route_sqlite).await;

    // 1. La requête rend des LIGNES sur PostgreSQL. `ou_defaut_journalise`
    //    avale les pannes SQL en rendant le défaut : sans cette assertion, une
    //    requête tombée passerait pour « aucune autre version ».
    let albums_pg = albums_rendus(&corps_pg);
    assert!(
        !albums_pg.is_empty(),
        "PostgreSQL ne rend AUCUNE version alors que la semence en garantit \
         cinq — la requête a échoué et sa panne a été avalée : {corps_pg}"
    );

    // 2. ⭐ Le cas de Gros Bidon, et l'artiste. Cinq versions : les quatre
    //    homonymes du même artiste plus le remaster à tiret. L'homonyme de
    //    « The Smooth Operators » n'en fait pas partie.
    assert_eq!(
        albums_pg.len(),
        5,
        "versions rendues sur PostgreSQL : {albums_pg:?}"
    );
    let titres_pg: Vec<&str> = corps_pg["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["title"].as_str().unwrap_or_default())
        .collect();
    assert!(
        titres_pg.contains(&"Smooth Operator - 2011 Remastered"),
        "le remaster à tiret n'est pas reconnu comme une version : {titres_pg:?}"
    );
    assert!(
        !albums_pg.iter().any(|a| a == "Ailleurs"),
        "l'homonyme d'un AUTRE artiste est sorti : {albums_pg:?}"
    );

    // 3. ⭐ Le remaster partage l'ISRC de la référence : il passe en tête,
    //    devant quatre albums qui, eux, ne sont départagés que par leur titre.
    assert_eq!(
        titres_pg.first(),
        Some(&"Smooth Operator - 2011 Remastered"),
        "le score ne classe pas : {titres_pg:?}"
    );

    // 4. ⭐ LA CONTRE-ÉPREUVE DES DEUX MOTEURS : même ensemble, même ORDRE.
    //    Les quatre titres d'album sont choisis pour que la collation binaire
    //    de SQLite et la collation locale de PostgreSQL divergent (cf.
    //    l'en-tête). Un `ORDER BY al.title` laissé au moteur ferait rougir
    //    ici.
    let albums_sqlite = albums_rendus(&corps_sqlite);
    assert_eq!(
        albums_pg, albums_sqlite,
        "les deux moteurs ne rangent pas les versions dans le même ordre"
    );

    sans_identifiants(&mut corps_pg);
    sans_identifiants(&mut corps_sqlite);
    assert_eq!(
        corps_pg, corps_sqlite,
        "PostgreSQL et SQLite ne rendent pas la même chose"
    );

    // 5. ⭐ LE TÉMOIN : un morceau sans autre version rend une liste VIDE, pas
    //    du bruit. Sans lui, une requête qui rendrait TOUT passerait les
    //    quatre points précédents.
    for (etat, nom) in [(&pg, "PostgreSQL"), (&sqlite, "SQLite")] {
        let route = format!(
            "/api/v1/library/tracks/{}/versions?streaming=false",
            id_de(etat, "/i2372/temoin.flac")
        );
        let temoin = corps_de(etat, &route).await;
        assert_eq!(
            temoin["versions"].as_array().map(Vec::len),
            Some(0),
            "{nom} : « Your Love Is King » n'a aucune autre version, et pourtant : {temoin}"
        );
    }
}
