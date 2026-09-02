//! Le drapeau « compilation » sort du serveur (#1957).
//!
//! L'issue disait « la table `albums` n'a aucune colonne pour lui ». Ce n'est
//! plus vrai : la colonne existe (SQLite `INTEGER`, PostgreSQL `SMALLINT` par
//! la migration 028), `album_repo` la lit, l'écrit et la met à jour, et le
//! scan la remplit depuis le tag `TCMP`. Le trou qui restait est un cran plus
//! loin : **les routes**. Celles qui passent par `Album::to_json()` rendaient
//! déjà le champ — par la sérialisation du modèle, sans qu'aucun `grep
//! is_compilation` sur `routes/` ne le montre. Celles qui bâtissent leur JSON
//! à la main, elles, le laissaient tomber en silence.
//!
//! C'est la famille « un chemin corrigé, les autres nus ». Ce fichier tient
//! donc un INVENTAIRE, pas un échantillon : toutes les routes qui servent un
//! album — celles qui étaient déjà servies comme celles qu'il a fallu
//! compléter — sont sondées ici, à travers le VRAI routeur, en lisant le
//! CORPS HTTP. Une transcription ne prouverait rien : avant le correctif,
//! `/library/albums-detailed` rendait 200 avec un album complet, sans le
//! drapeau, et personne ne rougissait.
//!
//! Trois albums témoins, dont un dont la colonne vaut `NULL` — l'état d'une
//! base migrée. La réponse doit dire `false`, **jamais `null`** : c'est la
//! décision de contrat, et elle est gardée ici.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

/// L'album dont le scan a levé le drapeau.
const COMPILATION: &str = "Anthologie du jazz";
/// L'album ordinaire — le drapeau doit valoir `false`, pas disparaître.
const ORDINAIRE: &str = "Kind of Blue";
/// L'album d'une base MIGRÉE : sa colonne est `NULL`. La réponse doit dire
/// `false`. Voir `drapeau_compilation` dans `tune-core`.
const MIGRE: &str = "Base migree sans drapeau";

/// Plancher du détecteur : une liste de sondes vidée par mégarde doit rougir,
/// pas passer à vide (même patron que `pg_routes_serveur.rs`).
const MINIMUM_DE_SONDES: usize = 11;

/// Les clés que `/library/albums-detailed` rendait AVANT #1957, plus la
/// nouvelle. Le témoin : on AJOUTE, on ne réorganise pas.
const CLES_ALBUMS_DETAILED: &[&str] = &[
    "album_id",
    "title",
    "album_artist",
    "cover_path",
    "label",
    "year",
    "duration_ms",
    "disc_count",
    "track_count",
    "format",
    "sample_rate",
    "bit_depth",
    "is_compilation",
];

/// Idem pour une collection intelligente.
const CLES_SMART_COLLECTION: &[&str] = &[
    "id",
    "title",
    "artist_name",
    "year",
    "cover_path",
    "genre",
    "track_count",
    "is_compilation",
];

/// Idem pour l'album qu'une playlist intelligente déduit de ses pistes.
const CLES_SMART_PLAYLIST: &[&str] = &[
    "album_id",
    "album_title",
    "artist_name",
    "cover_path",
    "year",
    "is_compilation",
];

// --- Plomberie HTTP : le corps, jamais une transcription ---

async fn get(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn post(app: &axum::Router, chemin: &str, corps: &Value) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Les trois albums témoins, plus une piste par album (les vues « détaillée »
/// et « playlist intelligente » agrègent depuis `tracks`).
///
/// L'insertion est écrite en SQL direct, et non par `AlbumRepo::create`, pour
/// pouvoir poser un `NULL` franc sur `is_compilation` — ce que le modèle, dont
/// le champ est un `bool`, ne permet pas d'exprimer.
fn seme(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) {
    backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis')",
            &[],
        )
        .expect("insertion d'artiste");
    for (id, titre, drapeau) in [
        (1i64, COMPILATION, "1"),
        (2, ORDINAIRE, "0"),
        (3, MIGRE, "NULL"),
    ] {
        backend
            .execute(
                &format!(
                    "INSERT INTO albums (id, title, artist_id, year, genre, source, is_compilation) \
                     VALUES ({id}, '{titre}', 1, 1959, 'Jazz', 'local', {drapeau})"
                ),
                &[],
            )
            .expect("insertion d'album");
        backend
            .execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, year) \
                     VALUES ({id}, 'piste {id}', {id}, 1, '/music/{id}.flac', 200000, 'flac', 1959)"
                ),
                &[],
            )
            .expect("insertion de piste");
    }
}

fn app_sqlite() -> axum::Router {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("etat serveur isole");
    seme(&etat.backend);
    tune_server::routes::router(etat)
}

/// Ramène les objets « album » d'une réponse, quelle que soit sa forme :
/// tableau nu, `{items:[…]}`, `{albums:[…]}`, `{local:{albums:[…]}}`, objet
/// unique, ou `{album:{…}}` (les albums les mieux notés).
fn albums_du_corps(corps: &Value) -> Vec<Value> {
    fn objets(v: &Value) -> Vec<Value> {
        match v {
            Value::Array(items) => items.iter().flat_map(objets).collect(),
            Value::Object(o) => {
                for cle in ["items", "albums"] {
                    if let Some(inner) = o.get(cle) {
                        return objets(inner);
                    }
                }
                if let Some(local) = o.get("local") {
                    return objets(local);
                }
                if let Some(album) = o.get("album") {
                    return objets(album);
                }
                if o.contains_key("title") || o.contains_key("album_title") {
                    return vec![v.clone()];
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
    objets(corps)
}

fn titre_de(album: &Value) -> Option<String> {
    album
        .get("title")
        .or_else(|| album.get("album_title"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Ce que le contrat exige pour chaque album témoin.
fn attendu(titre: &str) -> Option<Value> {
    match titre {
        COMPILATION => Some(json!(true)),
        // `NULL` en base ⇒ `false` dans la réponse, JAMAIS `null`.
        ORDINAIRE | MIGRE => Some(json!(false)),
        _ => None,
    }
}

/// Une route qui sert un album, et la façon de l'appeler.
struct Sonde {
    nom: &'static str,
    chemin: String,
    corps: Option<Value>,
}

impl Sonde {
    fn get(nom: &'static str, chemin: impl Into<String>) -> Self {
        Self {
            nom,
            chemin: chemin.into(),
            corps: None,
        }
    }
    fn post(nom: &'static str, chemin: impl Into<String>, corps: Value) -> Self {
        Self {
            nom,
            chemin: chemin.into(),
            corps: Some(corps),
        }
    }
}

/// L'INVENTAIRE. Chaque route de ce dépôt qui rend un album ou une liste
/// d'albums figure ici — celles qui servaient déjà le drapeau par le modèle
/// comme celles que #1957 a dû compléter. En retirer une, c'est rendre le
/// détecteur borgne ; le plancher `MINIMUM_DE_SONDES` l'interdit.
async fn sondes(app: &axum::Router) -> Vec<Sonde> {
    // Une collection et une playlist intelligentes, sans règle : toute la
    // bibliothèque. Créées par les VRAIES routes, pas par un INSERT.
    let (statut, collection) = post(
        app,
        "/api/v1/library/smart-collections",
        &json!({"name": "Tout", "rules": [], "match_mode": "all", "sort_by": "title", "sort_order": "asc"}),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::CREATED,
        "creation de collection intelligente: {collection}"
    );
    let id_collection = collection["id"].as_i64().expect("id de collection");

    let (statut, playlist) = post(
        app,
        "/api/v1/library/smart-playlists",
        &json!({"name": "Tout", "rules": [], "match_mode": "all", "sort_by": "title", "sort_order": "asc"}),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::CREATED,
        "creation de playlist intelligente: {playlist}"
    );
    let id_playlist = playlist["id"].as_i64().expect("id de playlist");

    vec![
        // --- servies par le modèle `Album` (aucune retouche #1957) ---
        Sonde::get("GET /library/albums", "/api/v1/library/albums?limit=50"),
        Sonde::get("GET /library/albums/{id}", "/api/v1/library/albums/1"),
        Sonde::get(
            "GET /library/albums/recent",
            "/api/v1/library/albums/recent",
        ),
        Sonde::get(
            "GET /library/recommendations",
            "/api/v1/library/recommendations",
        ),
        Sonde::get(
            "GET /library/artists/{id}/albums",
            "/api/v1/library/artists/1/albums",
        ),
        Sonde::get(
            "GET /library/artists/{id}/timeline",
            "/api/v1/library/artists/1/timeline",
        ),
        Sonde::get(
            "GET /library/genres/{name}/albums",
            "/api/v1/library/genres/Jazz/albums",
        ),
        Sonde::get("GET /library/search", "/api/v1/library/search?q=Kind"),
        // --- complétées par #1957 : le JSON y est bâti à la main ---
        Sonde::get(
            "GET /library/albums-detailed",
            "/api/v1/library/albums-detailed",
        ),
        Sonde::get(
            "GET /library/smart-collections/{id}/albums",
            format!("/api/v1/library/smart-collections/{id_collection}/albums"),
        ),
        Sonde::post(
            "POST /library/smart-collections/preview",
            "/api/v1/library/smart-collections/preview",
            json!({"rules": []}),
        ),
        Sonde::get(
            "GET /library/smart-playlists/{id}/albums",
            format!("/api/v1/library/smart-playlists/{id_playlist}/albums"),
        ),
    ]
}

// --- L'épreuve ---

#[tokio::test(flavor = "multi_thread")]
async fn i1957_toute_route_qui_sert_un_album_sert_son_drapeau_compilation() {
    let app = app_sqlite();
    let sondes = sondes(&app).await;
    assert!(
        sondes.len() >= MINIMUM_DE_SONDES,
        "l'inventaire des routes est tombe a {} (< {MINIMUM_DE_SONDES}) : \
         le detecteur passerait a vide",
        sondes.len()
    );

    let mut vus: std::collections::BTreeSet<String> = Default::default();
    for sonde in &sondes {
        let (statut, corps) = match &sonde.corps {
            Some(c) => post(&app, &sonde.chemin, c).await,
            None => get(&app, &sonde.chemin).await,
        };
        assert_eq!(
            statut,
            StatusCode::OK,
            "{} ({}) doit repondre 200 — corps={corps}",
            sonde.nom,
            sonde.chemin
        );
        let albums = albums_du_corps(&corps);
        assert!(
            !albums.is_empty(),
            "{} ne rend aucun album : la sonde ne prouverait rien — corps={corps}",
            sonde.nom
        );
        let mut temoins = 0usize;
        for album in &albums {
            let Some(titre) = titre_de(album) else {
                continue;
            };
            let Some(attendu) = attendu(&titre) else {
                continue;
            };
            temoins += 1;
            vus.insert(titre.clone());
            let rendu = album.get("is_compilation");
            assert!(
                rendu.is_some(),
                "{} ne sert PAS `is_compilation` pour « {titre} » — album={album}",
                sonde.nom
            );
            let rendu = rendu.unwrap();
            assert!(
                rendu.is_boolean(),
                "{} rend `is_compilation` = {rendu} pour « {titre} » : le contrat \
                 est un booleen, jamais `null` (une base migree porte des NULL)",
                sonde.nom
            );
            assert_eq!(
                rendu, &attendu,
                "{} rend `is_compilation` = {rendu} pour « {titre} », attendu {attendu}",
                sonde.nom
            );
        }
        assert!(
            temoins > 0,
            "{} n'a rendu aucun des albums temoins : la sonde ne prouve rien — corps={corps}",
            sonde.nom
        );
    }

    // Les trois états — vrai, faux, et la colonne NULL d'une base migrée —
    // ont chacun été traversés par au moins une route.
    for titre in [COMPILATION, ORDINAIRE, MIGRE] {
        assert!(
            vus.contains(titre),
            "aucune sonde n'a rendu « {titre} » : l'etat qu'il porte n'est pas garde"
        );
    }
}

/// Le témoin : les réponses ne PERDENT rien. On ajoute une clé, on n'en
/// déplace ni n'en retire aucune.
#[tokio::test(flavor = "multi_thread")]
async fn i1957_les_reponses_album_ne_perdent_aucune_cle() {
    let app = app_sqlite();

    let (_, corps) = get(&app, "/api/v1/library/albums-detailed").await;
    let item = albums_du_corps(&corps)
        .into_iter()
        .next()
        .expect("un album detaille");
    verifie_les_cles("GET /library/albums-detailed", &item, CLES_ALBUMS_DETAILED);

    let (_, collection) = post(
        &app,
        "/api/v1/library/smart-collections/preview",
        &json!({"rules": []}),
    )
    .await;
    let item = albums_du_corps(&collection)
        .into_iter()
        .next()
        .expect("un album de collection");
    verifie_les_cles(
        "POST /library/smart-collections/preview",
        &item,
        CLES_SMART_COLLECTION,
    );

    let (statut, playlist) = post(
        &app,
        "/api/v1/library/smart-playlists",
        &json!({"name": "Tout", "rules": []}),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{playlist}");
    let id = playlist["id"].as_i64().expect("id de playlist");
    let (_, corps) = get(
        &app,
        &format!("/api/v1/library/smart-playlists/{id}/albums"),
    )
    .await;
    let item = albums_du_corps(&corps)
        .into_iter()
        .next()
        .expect("un album de playlist intelligente");
    verifie_les_cles(
        "GET /library/smart-playlists/{id}/albums",
        &item,
        CLES_SMART_PLAYLIST,
    );

    // Le modèle `Album` porte le champ dans SA sérialisation : la liste
    // d'albums doit garder ses clés historiques ET le drapeau.
    let (_, corps) = get(&app, "/api/v1/library/albums?limit=1").await;
    let item = albums_du_corps(&corps)
        .into_iter()
        .next()
        .expect("un album de la liste");
    for cle in ["id", "title", "artist_name", "year", "genre", "quality"] {
        assert!(
            item.get(cle).is_some(),
            "GET /library/albums a perdu la cle « {cle} » — album={item}"
        );
    }
    assert_eq!(item["is_compilation"], json!(true));
}

fn verifie_les_cles(route: &str, item: &Value, attendues: &[&str]) {
    let objet = item.as_object().expect("un objet JSON");
    let rendues: std::collections::BTreeSet<&str> = objet.keys().map(String::as_str).collect();
    let attendues: std::collections::BTreeSet<&str> = attendues.iter().copied().collect();
    let manquantes: Vec<_> = attendues.difference(&rendues).collect();
    let en_trop: Vec<_> = rendues.difference(&attendues).collect();
    assert!(
        manquantes.is_empty() && en_trop.is_empty(),
        "{route} : cles manquantes {manquantes:?}, cles inattendues {en_trop:?} — item={item}"
    );
}

/// `?compilation=true` / `?compilation=false` filtrent DÉJÀ la liste (#1957) :
/// le drapeau servi et le filtre offert disent donc la même chose. Sans cette
/// épreuve, un client pourrait afficher une pastille que le filtre contredit.
#[tokio::test(flavor = "multi_thread")]
async fn i1957_le_filtre_compilation_dit_la_meme_chose_que_le_drapeau_servi() {
    let app = app_sqlite();
    for (parametre, attendu_flag) in [("true", true), ("false", false)] {
        let (statut, corps) = get(
            &app,
            &format!("/api/v1/library/albums?limit=50&compilation={parametre}"),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        let albums = albums_du_corps(&corps);
        assert!(
            !albums.is_empty(),
            "?compilation={parametre} ne rend rien : le filtre ne prouverait rien"
        );
        for album in &albums {
            assert_eq!(
                album["is_compilation"],
                json!(attendu_flag),
                "?compilation={parametre} rend un album dont le drapeau dit le contraire — {album}"
            );
        }
    }
}

// --- PostgreSQL : le même contrat, sur l'autre moteur ---
//
// ⚠️ Doctrine de `pg_routes_serveur.rs`, et NON celle de `pg_or_skip!` :
// la variable ABSENTE saute (un `cargo test` ordinaire n'a pas de base), mais
// une variable POSÉE dont la connexion échoue fait TOMBER le test. `pg_or_skip!`
// rend `None` dans les deux cas, si bien qu'un banc mal branché s'affiche vert.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn url_pg() -> Option<String> {
        std::env::var("TUNE_TEST_PG_URL").ok()
    }

    fn etat_postgres(url: &str) -> AppState {
        let config = tune_server::config::TuneConfig {
            database_url: Some(url.to_string()),
            ..Default::default()
        };
        // Pas de `ok()?` : une connexion qui échoue doit ROUGIR, jamais sauter.
        AppState::new("", 0, config).expect("AppState sur PostgreSQL")
    }

    /// Le type RÉEL de la colonne sur PostgreSQL. `pg_migrate` pose toutes les
    /// colonnes en TEXT sur les bases nées de `migrate-to-postgres` ; la
    /// migration 028 les ramène en `SMALLINT`. Un booléen lu comme texte se
    /// comporte mal en silence — on le mesure au lieu de le supposer.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_i1957_la_colonne_est_un_entier_et_les_routes_servent_le_drapeau() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — epreuve PostgreSQL sautee");
            return;
        };
        let etat = etat_postgres(&url);

        let type_reel = etat
            .backend
            .query_one(
                "SELECT data_type FROM information_schema.columns \
                 WHERE table_name = 'albums' AND column_name = 'is_compilation'",
                &[],
            )
            .expect("lecture du type de colonne")
            .and_then(|r| r.first().and_then(|v| v.as_string()))
            .expect("la colonne is_compilation doit exister sur PostgreSQL");
        assert_eq!(
            type_reel, "smallint",
            "is_compilation est en « {type_reel} » sur PostgreSQL : la migration 028 \
             n'a pas ramene la colonne en SMALLINT, et un booleen lu comme texte \
             se comporte mal en silence"
        );

        for table in ["tracks", "albums", "artists"] {
            etat.backend
                .execute(&format!("DELETE FROM {table}"), &[])
                .expect("vidage");
        }
        seme(&etat.backend);
        let app = tune_server::routes::router(etat);

        for sonde in sondes(&app).await {
            let (statut, corps) = match &sonde.corps {
                Some(c) => post(&app, &sonde.chemin, c).await,
                None => get(&app, &sonde.chemin).await,
            };
            assert_eq!(statut, StatusCode::OK, "{} — corps={corps}", sonde.nom);
            let mut temoins = 0usize;
            for album in albums_du_corps(&corps) {
                let Some(titre) = titre_de(&album) else {
                    continue;
                };
                let Some(attendu) = attendu(&titre) else {
                    continue;
                };
                temoins += 1;
                assert_eq!(
                    album.get("is_compilation"),
                    Some(&attendu),
                    "{} rend {:?} pour « {titre} » sur PostgreSQL",
                    sonde.nom,
                    album.get("is_compilation")
                );
            }
            assert!(
                temoins > 0,
                "{} ne rend aucun temoin sur PostgreSQL",
                sonde.nom
            );
        }
    }
}
