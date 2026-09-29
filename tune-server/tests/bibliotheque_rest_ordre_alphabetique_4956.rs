//! #4956, suite décidée par Bertrand le 29/09/2026 : l'API REST de la
//! bibliothèque — celle que le client web lit PAR PAGES (#4800) — range ses
//! listes dans le MÊME ordre alphabétique que le serveur média (PR #5401,
//! `comparer_alphabetique`) : signes de tête ignorés, casse et accents
//! ignorés, nombres par leur valeur, ex æquo départagés de façon stable.
//!
//! Avant, `GET /library/artists`, `GET /library/albums?sort=title|artist` et
//! `GET /playlists` triaient en SQL par `ORDER BY LOWER(…)`. Sur SQLite,
//! `LOWER` ne replie que l'ASCII : « Édith Piaf » passait après « ZZ Top »,
//! « (hed) p.e. » et « 'Til Tuesday » sortaient en tête ; sur PostgreSQL,
//! l'ordre suivait la collation de la base — un autre encore.
//!
//! Ce que le témoin exige, sur les vraies routes :
//! 1. l'ordre attendu (les cas pièges de #5401, et « Édith Piaf » avant
//!    « ZZ Top ») ;
//! 2. la pagination : pages de 1, 2, 3 et 5, aucune ne perd ni ne répète
//!    d'élément, et `total` est la taille de la liste ;
//! 3. le saut par lettre du rail A–Z en mode paginé : la dichotomie du
//!    client web (`offsetDeLettre`, requêtes `limit=1`) tombe sur le premier
//!    élément de chaque lettre, ce qui suppose que l'initiale croisse le long
//!    de la liste.
//!
//! Le même scénario tourne sur SQLite et sur PostgreSQL (`TUNE_TEST_PG_URL`).

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

async fn corps(state: &AppState, route: &str) -> Value {
    let rep = tune_server::routes::router(state.clone())
        .oneshot(
            Request::get(route)
                .header("X-Profile-Id", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(rep.status().is_success(), "{route} : {}", rep.status());
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 24)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

/// (artiste, nom de tri, titre de l'album, année). Créés dans un ordre
/// QUELCONQUE : l'ordre attendu ne peut pas venir de l'ordre d'insertion.
const BIBLIOTHEQUE: &[(&str, Option<&str>, &str, i64)] = &[
    ("ZZ Top", None, "Zoo", 1990),
    ("(hed) p.e.", None, "(Inédit)", 2000),
    ("Édith Piaf", None, "Été indien", 1960),
    ("edith Crash", None, "été 85", 1985),
    ("'Til Tuesday", None, "'Round Midnight", 1985),
    ("The Beatles", Some("Beatles, The"), "Album 10", 1970),
    ("Zazie", None, "Zen", 1995),
    ("Aphex Twin", None, "Ambient Works", 1992),
    ("2Pac", None, "2 Tone", 1996),
    // Un nom de tri blanc ne compte pas : c'est le nom qui trie.
    ("Nina Simone", Some("   "), "Album 9", 1958),
    ("10cc", None, "70s", 1975),
    // Deuxième « Zoo », d'un autre artiste : ex æquo, départagé par l'id.
    ("Zazie", None, "Zoo", 1997),
];

const ARTISTES_ATTENDUS: &[&str] = &[
    "2Pac",
    "10cc",
    "Aphex Twin",
    "The Beatles",
    "edith Crash",
    "Édith Piaf",
    "(hed) p.e.",
    "Nina Simone",
    "'Til Tuesday",
    "Zazie",
    "ZZ Top",
];

const TITRES_ATTENDUS: &[&str] = &[
    "2 Tone",
    "70s",
    "Album 9",
    "Album 10",
    "Ambient Works",
    "été 85",
    "Été indien",
    "(Inédit)",
    "'Round Midnight",
    "Zen",
    "Zoo",
    "Zoo",
];

/// Par artiste (nom de l'artiste, pas son nom de tri : c'est ce que la
/// grille affiche et ce que son rail lit), puis année, puis titre.
const ALBUMS_PAR_ARTISTE_ATTENDUS: &[(&str, &str)] = &[
    ("2Pac", "2 Tone"),
    ("10cc", "70s"),
    ("Aphex Twin", "Ambient Works"),
    ("edith Crash", "été 85"),
    ("Édith Piaf", "Été indien"),
    ("(hed) p.e.", "(Inédit)"),
    ("Nina Simone", "Album 9"),
    ("The Beatles", "Album 10"),
    ("'Til Tuesday", "'Round Midnight"),
    ("Zazie", "Zen"),
    ("Zazie", "Zoo"),
    ("ZZ Top", "Zoo"),
];

const LISTES: &[&str] = &["Zen", "(Soirée)", "Été", "apéro", "10 ans", "9 vies"];
const LISTES_ATTENDUES: &[&str] = &["9 vies", "10 ans", "apéro", "Été", "(Soirée)", "Zen"];

/// Sème la bibliothèque. Rend les identifiants des deux « Zoo », dans
/// l'ordre de création.
fn semer(state: &AppState) -> (i64, i64) {
    let artistes = ArtistRepo::with_backend(state.backend.clone());
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let pistes = TrackRepo::with_backend(state.backend.clone());
    let listes = PlaylistRepo::with_backend(state.backend.clone());
    let mut par_nom = std::collections::HashMap::new();
    let mut zoos = Vec::new();
    let mut une_piste = None;
    for (i, (artiste, tri, titre, annee)) in BIBLIOTHEQUE.iter().enumerate() {
        let artiste_id = *par_nom.entry(*artiste).or_insert_with(|| {
            let mut a = Artist::new(artiste.to_string());
            a.sort_name = tri.map(str::to_string);
            artistes.create(&a).unwrap()
        });
        let mut album = Album::new(titre.to_string());
        album.year = Some(*annee as i32);
        album.artist_id = Some(artiste_id);
        album.artist_name = Some(artiste.to_string());
        let album_id = albums.create(&album).unwrap();
        if *titre == "Zoo" {
            zoos.push(album_id);
        }
        let mut piste = Track::new(format!("Piste {i}"));
        piste.album_id = Some(album_id);
        piste.album_title = Some(titre.to_string());
        piste.artist_id = Some(artiste_id);
        piste.artist_name = Some(artiste.to_string());
        piste.file_path = Some(format!("/musique/ordre-4956/{i}.flac"));
        une_piste.get_or_insert(pistes.create(&piste).unwrap());
    }
    for nom in LISTES {
        listes
            .create_with_tracks(nom, None, 1, &[une_piste.unwrap()])
            .unwrap();
    }
    (zoos[0], zoos[1])
}

/// Toute la liste d'une route paginée, page par page (`limit` = `taille`).
/// Rend les éléments et le `total` annoncé par la première page (`None` pour
/// `GET /playlists`, qui rend un tableau nu).
async fn pages(state: &AppState, route: &str, taille: usize) -> (Vec<Value>, Option<u64>) {
    let sep = if route.contains('?') { '&' } else { '?' };
    let mut tout = Vec::new();
    let mut total = None;
    for page in 0.. {
        let r = corps(
            state,
            &format!("{route}{sep}limit={taille}&offset={}", page * taille),
        )
        .await;
        let items = match &r {
            Value::Array(a) => a.clone(),
            _ => {
                total.get_or_insert(r["total"].as_u64().expect("total"));
                r["items"].as_array().expect("items").clone()
            }
        };
        let n = items.len();
        tout.extend(items);
        if n < taille {
            break;
        }
        assert!(page < 100, "{route} : pagination sans fin");
    }
    (tout, total)
}

fn champ<'a>(items: &'a [Value], cle: &str) -> Vec<&'a str> {
    items
        .iter()
        .map(|v| v[cle].as_str().unwrap_or(""))
        .collect()
}

fn ids(items: &[Value]) -> Vec<i64> {
    items
        .iter()
        .map(|v| v["id"].as_i64().expect("id"))
        .collect()
}

/// L'initiale du rail : la première lettre ou le premier chiffre après les
/// signes de tête, sans accent, en capitale ; « # » hors A–Z. C'est la règle
/// d'`initialeAlphabetique` du client web (#1772).
fn initiale(s: &str) -> char {
    let c = s
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .unwrap_or('#');
    let c = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        c => c,
    };
    if c.is_ascii_lowercase() {
        c.to_ascii_uppercase()
    } else {
        '#'
    }
}

fn rang(c: char) -> u32 {
    if c.is_ascii_uppercase() {
        c as u32 - 'A' as u32 + 1
    } else {
        0
    }
}

/// Le saut par lettre du client web en mode paginé (`offsetDeLettre`,
/// `albumsPagines.ts`) : dichotomie sur `offset` avec `limit=1`, jusqu'au
/// premier élément dont l'initiale vaut AU MOINS la lettre. Rend l'offset.
async fn offset_de_lettre(
    state: &AppState,
    route: &str,
    total: usize,
    lettre: char,
    cle: &dyn Fn(&Value) -> String,
) -> usize {
    let sep = if route.contains('?') { '&' } else { '?' };
    let cible = rang(lettre);
    let (mut lo, mut hi) = (0usize, total);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let r = corps(state, &format!("{route}{sep}limit=1&offset={mid}")).await;
        let item = &r["items"][0];
        assert!(!item.is_null(), "{route} : offset {mid} vide sur {total}");
        if rang(initiale(&cle(item))) >= cible {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo.min(total - 1)
}

/// Vérifie le rail d'une liste : l'initiale croît le long de la liste, et
/// chaque lettre présente est trouvée par la dichotomie à sa première place.
async fn rail(
    state: &AppState,
    route: &str,
    liste: &[Value],
    cle: &dyn Fn(&Value) -> String,
    ecarts: &mut Vec<String>,
) {
    let rangs: Vec<u32> = liste.iter().map(|v| rang(initiale(&cle(v)))).collect();
    if rangs.windows(2).any(|w| w[0] > w[1]) {
        let initiales: Vec<char> = liste.iter().map(|v| initiale(&cle(v))).collect();
        ecarts.push(format!("{route} : initiales non croissantes {initiales:?}"));
        return;
    }
    for lettre in ['A', 'B', 'E', 'H', 'N', 'R', 'T', 'Z'] {
        let attendu = rangs
            .iter()
            .position(|&r| r >= rang(lettre))
            .unwrap_or(liste.len() - 1);
        let obtenu = offset_de_lettre(state, route, liste.len(), lettre, cle).await;
        if obtenu != attendu {
            ecarts.push(format!(
                "{route} : saut à {lettre} → offset {obtenu}, attendu {attendu}"
            ));
        }
    }
}

/// Le scénario, commun aux deux moteurs. Rend les écarts constatés.
async fn scenario(state: &AppState) -> Vec<String> {
    let (zoo_1, zoo_2) = semer(state);
    let mut ecarts = Vec::new();

    // ── Artistes ────────────────────────────────────────────────────────
    let tous = corps(state, "/api/v1/library/artists?limit=1000").await;
    let artistes = tous["items"].as_array().unwrap().clone();
    if champ(&artistes, "name") != ARTISTES_ATTENDUS {
        ecarts.push(format!("artistes : {:?}", champ(&artistes, "name")));
    }
    for taille in [1, 2, 3, 5] {
        let (pagine, total) = pages(state, "/api/v1/library/artists", taille).await;
        if ids(&pagine) != ids(&artistes) || total != Some(artistes.len() as u64) {
            ecarts.push(format!(
                "artistes par pages de {taille} : {:?} (total {total:?})",
                champ(&pagine, "name")
            ));
        }
    }
    let cle_artiste = |v: &Value| {
        v["sort_name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or(v["name"].as_str())
            .unwrap_or("")
            .to_string()
    };
    rail(
        state,
        "/api/v1/library/artists",
        &artistes,
        &cle_artiste,
        &mut ecarts,
    )
    .await;

    // ── Albums par titre, dans les deux sens ────────────────────────────
    let route_titre = "/api/v1/library/albums?sort=title&order=asc";
    let par_titre = corps(state, &format!("{route_titre}&limit=1000")).await;
    let albums = par_titre["items"].as_array().unwrap().clone();
    if champ(&albums, "title") != TITRES_ATTENDUS {
        ecarts.push(format!("albums par titre : {:?}", champ(&albums, "title")));
    }
    let zoos: Vec<i64> = albums
        .iter()
        .filter(|a| a["title"] == "Zoo")
        .map(|a| a["id"].as_i64().unwrap())
        .collect();
    if zoos != [zoo_1.min(zoo_2), zoo_1.max(zoo_2)] {
        ecarts.push(format!("ex æquo « Zoo » : {zoos:?}"));
    }
    let desc = corps(
        state,
        "/api/v1/library/albums?sort=title&order=desc&limit=1000",
    )
    .await;
    let desc = desc["items"].as_array().unwrap().clone();
    let mut attendu_desc: Vec<&str> = TITRES_ATTENDUS.to_vec();
    attendu_desc.reverse();
    // En décroissant, seule la clé se renverse : les deux « Zoo » restent
    // départagés par l'id croissant, comme le `a.id ASC` du SQL d'avant.
    let zoos_desc: Vec<i64> = desc
        .iter()
        .filter(|a| a["title"] == "Zoo")
        .map(|a| a["id"].as_i64().unwrap())
        .collect();
    if champ(&desc, "title") != attendu_desc || zoos_desc != zoos {
        ecarts.push(format!(
            "albums par titre décroissant : {:?} ({zoos_desc:?})",
            champ(&desc, "title")
        ));
    }
    for taille in [1, 2, 3, 5] {
        for route in [route_titre, "/api/v1/library/albums?sort=title&order=desc"] {
            let (pagine, total) = pages(state, route, taille).await;
            let entiere = corps(state, &format!("{route}&limit=1000")).await;
            let entiere = entiere["items"].as_array().unwrap().clone();
            if ids(&pagine) != ids(&entiere) || total != Some(entiere.len() as u64) {
                ecarts.push(format!(
                    "{route} par pages de {taille} : {:?} (total {total:?})",
                    champ(&pagine, "title")
                ));
            }
        }
    }
    let cle_titre = |v: &Value| v["title"].as_str().unwrap_or("").to_string();
    rail(state, route_titre, &albums, &cle_titre, &mut ecarts).await;

    // ── Albums par artiste ──────────────────────────────────────────────
    let route_artiste = "/api/v1/library/albums?sort=artist&order=asc";
    let par_artiste = corps(state, &format!("{route_artiste}&limit=1000")).await;
    let par_artiste = par_artiste["items"].as_array().unwrap().clone();
    let obtenu: Vec<(&str, &str)> = champ(&par_artiste, "artist_name")
        .into_iter()
        .zip(champ(&par_artiste, "title"))
        .collect();
    if obtenu != ALBUMS_PAR_ARTISTE_ATTENDUS {
        ecarts.push(format!("albums par artiste : {obtenu:?}"));
    }
    for taille in [2, 3] {
        let (pagine, total) = pages(state, route_artiste, taille).await;
        if ids(&pagine) != ids(&par_artiste) || total != Some(par_artiste.len() as u64) {
            ecarts.push(format!(
                "albums par artiste, pages de {taille} : {:?}",
                champ(&pagine, "title")
            ));
        }
    }
    let cle_artiste_album = |v: &Value| v["artist_name"].as_str().unwrap_or("").to_string();
    rail(
        state,
        route_artiste,
        &par_artiste,
        &cle_artiste_album,
        &mut ecarts,
    )
    .await;

    // ── Listes de lecture ───────────────────────────────────────────────
    let listes = corps(state, "/api/v1/playlists?limit=1000").await;
    let listes = listes.as_array().unwrap().clone();
    if champ(&listes, "name") != LISTES_ATTENDUES {
        ecarts.push(format!("listes : {:?}", champ(&listes, "name")));
    }
    for taille in [1, 4] {
        let (pagine, _) = pages(state, "/api/v1/playlists", taille).await;
        if ids(&pagine) != ids(&listes) {
            ecarts.push(format!(
                "listes par pages de {taille} : {:?}",
                champ(&pagine, "name")
            ));
        }
    }

    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn listes_rest_dans_l_ordre_alphabetique_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_listes_rest_dans_l_ordre_alphabetique() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    // Une base de test partagée : on repart d'une bibliothèque vide.
    state
        .backend
        .execute(
            "TRUNCATE playlist_tracks, playlists, tracks, albums, artists RESTART IDENTITY CASCADE",
            &[],
        )
        .expect("vidage de la bibliothèque PostgreSQL");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
