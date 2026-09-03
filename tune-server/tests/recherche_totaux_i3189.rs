//! #3189 — « Pistes 50 » n'était pas un compte, c'était la limite.
//!
//! jfpaquet, forum fil 1644 (02/09/2026), 0.9.130 Windows/PostgreSQL,
//! 77 291 pistes : il cherche « Autumn Leaves », Tune annonce « Pistes 50 »,
//! et Everything en trouve 58 dans UN de ses dossiers, 52 dans un autre.
//!
//! `GET /search` ne rendait ni total, ni `has_more`, ni pagination : l'écran
//! affichait `filteredTracks.length`, c'est-à-dire la longueur de ce que le
//! serveur avait bien voulu envoyer. Le nombre affiché était donc TOUJOURS
//! `min(correspondances, limit)`, et rien — pas une clé, pas un drapeau — ne
//! disait que la liste était coupée.
//!
//! Ce fichier tient les quatre affirmations qui font qu'un total est un total :
//!
//!   1. [`le_total_est_le_nombre_de_correspondances_pas_la_longueur_de_la_liste`]
//!      — plus de correspondances que la limite : le total les compte toutes.
//!   2. [`le_temoin_sous_la_limite_dit_le_meme_nombre_et_pas_de_suite`] — le
//!      témoin. Sans lui, un total qui dirait n'importe quoi (le nombre de
//!      pistes de la base, une constante) passerait l'épreuve 1.
//!   3. [`la_pagination_va_au_bout_sans_doublon_ni_trou`] — parcourir les
//!      pages rend l'ensemble, une fois chacune. C'est ce qui exige un
//!      `ORDER BY` TOTAL ; la requête n'en portait aucun.
//!   4. [`albums_et_artistes_ont_leur_propre_total`] — trois compteurs
//!      distincts, et non le même nombre recopié trois fois.
//!
//! Et deux affirmations de non-régression :
//!
//!   5. [`la_forme_que_lit_un_client_0_9_130_est_intacte`] — `local.tracks`
//!      reste un tableau au même endroit, `radios` et `services` restent là.
//!   6. [`sans_offset_la_premiere_page_est_celle_d_avant`] — le défaut ne
//!      bouge pas.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashSet;
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::track_repo::TrackRepo;

// --- socle -------------------------------------------------------------

/// Trois nombres DIFFÉRENTS, et tous supérieurs à la limite éprouvée : un
/// total recopié d'une famille sur l'autre se voit tout de suite.
const PISTES: usize = 137;
const ALBUMS: usize = 61;
const ARTISTES: usize = 43;

/// Le mot cherché. Celui du signalement.
const MOT: &str = "Autumn";

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Une bibliothèque où « Autumn » a strictement plus de correspondances que
/// la limite que la route emploiera, dans les trois familles.
///
/// Les pistes n'ont ni album ni artiste : leur prédicat ne peut donc pas
/// attraper au passage les albums et artistes créés ici, et les trois totaux
/// restent indépendants.
fn bibliotheque(state: &tune_server::state::AppState) {
    let tracks = TrackRepo::with_backend(state.backend.clone());
    for i in 0..PISTES {
        let mut t = Track::new(format!("{MOT} Leaves {i:04}"));
        t.file_path = Some(format!("/musique/{MOT}-{i:04}.flac"));
        tracks.create(&t).expect("insert piste");
    }
    let albums = AlbumRepo::with_backend(state.backend.clone());
    for i in 0..ALBUMS {
        albums
            .create(&Album::new(format!("{MOT} Sessions {i:04}")))
            .expect("insert album");
    }
    let artists = ArtistRepo::with_backend(state.backend.clone());
    for i in 0..ARTISTES {
        artists
            .create(&Artist::new(format!("{MOT} Trio {i:04}")))
            .expect("insert artiste");
    }
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// `GET /api/v1/search` avec la requête, la limite et, éventuellement, le rang.
async fn chercher(state: &tune_server::state::AppState, limite: i64, offset: Option<i64>) -> Value {
    let app = tune_server::routes::router(state.clone());
    let mut url = format!("/api/v1/search?q={MOT}&limit={limite}");
    if let Some(o) = offset {
        url.push_str(&format!("&offset={o}"));
    }
    let (statut, corps) = get(&app, &url).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "GET {url} devait répondre 200, corps : {corps}"
    );
    corps
}

fn entier(v: &Value, chemin: &[&str]) -> i64 {
    let mut cur = v;
    for cle in chemin {
        cur = cur
            .get(*cle)
            .unwrap_or_else(|| panic!("clé « {cle} » absente de la réponse : {v}"));
    }
    cur.as_i64()
        .unwrap_or_else(|| panic!("{chemin:?} n'est pas un entier : {cur}"))
}

fn booleen(v: &Value, chemin: &[&str]) -> bool {
    let mut cur = v;
    for cle in chemin {
        cur = cur
            .get(*cle)
            .unwrap_or_else(|| panic!("clé « {cle} » absente de la réponse : {v}"));
    }
    cur.as_bool()
        .unwrap_or_else(|| panic!("{chemin:?} n'est pas un booléen : {cur}"))
}

fn pistes(v: &Value) -> &Vec<Value> {
    v["local"]["tracks"]
        .as_array()
        .unwrap_or_else(|| panic!("local.tracks n'est pas un tableau : {v}"))
}

// --- 1. le mensonge ----------------------------------------------------

/// Le défaut signalé, en une assertion : le nombre annoncé ne doit PAS être
/// la longueur de la liste rendue.
#[tokio::test]
async fn le_total_est_le_nombre_de_correspondances_pas_la_longueur_de_la_liste() {
    let state = etat();
    bibliotheque(&state);

    // 50 : la limite exacte de l'écran de jfpaquet.
    let corps = chercher(&state, 50, None).await;

    assert_eq!(
        pistes(&corps).len(),
        50,
        "la liste rendue est bornée par `limit`, c'est son rôle"
    );
    assert_eq!(
        entier(&corps, &["local", "totals", "tracks"]),
        PISTES as i64,
        "le total doit être le NOMBRE DE CORRESPONDANCES ({PISTES}), \
         pas la longueur de la liste (50)"
    );
    assert!(
        booleen(&corps, &["local", "has_more", "tracks"]),
        "87 pistes n'ont pas été rendues : `has_more` doit le dire"
    );
    assert!(
        !booleen(&corps, &["local", "totals_capped", "tracks"]),
        "{PISTES} est très en dessous du plafond de comptage : le total est exact"
    );
}

// --- 2. le témoin ------------------------------------------------------

/// Le contre-poids de l'épreuve 1 : quand tout tient sous la limite, le total
/// vaut la longueur, et il n'y a PAS de suite.
///
/// Sans ce témoin, un « total » qui rendrait le nombre de pistes de la base,
/// ou n'importe quelle constante supérieure à la limite, passerait l'épreuve
/// précédente sans rien prouver.
#[tokio::test]
async fn le_temoin_sous_la_limite_dit_le_meme_nombre_et_pas_de_suite() {
    let state = etat();
    // Sept pistes seulement, et une limite de 50 : la liste est complète.
    let tracks = TrackRepo::with_backend(state.backend.clone());
    for i in 0..7 {
        let mut t = Track::new(format!("{MOT} Leaves {i:04}"));
        t.file_path = Some(format!("/musique/{MOT}-{i:04}.flac"));
        tracks.create(&t).expect("insert piste");
    }
    // Du bruit qui ne correspond pas : un total qui compterait la table
    // entière le ramasserait.
    for i in 0..40 {
        let mut t = Track::new(format!("Winter Sun {i:04}"));
        t.file_path = Some(format!("/musique/winter-{i:04}.flac"));
        tracks.create(&t).expect("insert piste hors sujet");
    }

    let corps = chercher(&state, 50, None).await;

    assert_eq!(pistes(&corps).len(), 7, "sept correspondances, sept lignes");
    assert_eq!(
        entier(&corps, &["local", "totals", "tracks"]),
        7,
        "sous la limite, le total vaut la longueur — et surtout pas 47"
    );
    assert!(
        !booleen(&corps, &["local", "has_more", "tracks"]),
        "rien n'a été coupé : `has_more` doit être faux"
    );
}

// --- 3. la pagination --------------------------------------------------

/// Parcourir toutes les pages rend EXACTEMENT l'ensemble : ni doublon, ni trou.
///
/// C'est l'épreuve qui exige un ordre TOTAL. La requête de recherche ne portait
/// aucun `ORDER BY` : les deux moteurs étaient libres de rendre les lignes dans
/// l'ordre qui les arrange, et une ligne vue page 1 pouvait reparaître page 2
/// pendant qu'une autre ne paraissait jamais.
#[tokio::test]
async fn la_pagination_va_au_bout_sans_doublon_ni_trou() {
    let state = etat();
    bibliotheque(&state);

    let limite = 20_i64;
    let mut vus: Vec<i64> = Vec::new();
    let mut offset = 0_i64;
    // Borne de sûreté : une pagination qui n'avance pas doit faire échouer le
    // test, pas tourner sans fin.
    for _ in 0..50 {
        let corps = chercher(&state, limite, Some(offset)).await;
        assert_eq!(
            entier(&corps, &["local", "offset"]),
            offset,
            "la réponse doit dire à quel rang elle commence"
        );
        let page = pistes(&corps);
        for p in page {
            vus.push(p["id"].as_i64().expect("piste sans id"));
        }
        let suite = booleen(&corps, &["local", "has_more", "tracks"]);
        if !suite {
            assert!(
                page.len() as i64 <= limite,
                "une page ne dépasse jamais la limite"
            );
            break;
        }
        assert!(
            !page.is_empty(),
            "`has_more` vrai et page vide : la pagination n'avance pas"
        );
        offset += limite;
    }

    let uniques: HashSet<i64> = vus.iter().copied().collect();
    assert_eq!(
        vus.len(),
        uniques.len(),
        "une piste est revenue deux fois : l'ordre n'est pas total"
    );
    assert_eq!(
        uniques.len(),
        PISTES,
        "le parcours complet doit rendre les {PISTES} correspondances, \
         il en a rendu {}",
        uniques.len()
    );

    // Et l'ensemble parcouru est bien celui que le total annonçait.
    let premiere = chercher(&state, limite, Some(0)).await;
    assert_eq!(
        entier(&premiere, &["local", "totals", "tracks"]),
        uniques.len() as i64,
        "le total annoncé et l'ensemble réellement parcouru doivent coïncider"
    );
}

// --- 4. trois familles, trois compteurs --------------------------------

#[tokio::test]
async fn albums_et_artistes_ont_leur_propre_total() {
    let state = etat();
    bibliotheque(&state);

    let corps = chercher(&state, 10, None).await;

    assert_eq!(
        entier(&corps, &["local", "totals", "albums"]),
        ALBUMS as i64,
        "les albums ont leur propre compte"
    );
    assert_eq!(
        entier(&corps, &["local", "totals", "artists"]),
        ARTISTES as i64,
        "les artistes aussi"
    );
    assert_eq!(
        entier(&corps, &["local", "totals", "tracks"]),
        PISTES as i64
    );
    for famille in ["albums", "artists", "tracks"] {
        assert!(
            booleen(&corps, &["local", "has_more", famille]),
            "{famille} : dix lignes rendues sur bien plus, `has_more` doit être vrai"
        );
    }

    // Et la pagination des albums avance aussi.
    let page2 = chercher(&state, 10, Some(10)).await;
    let ids1: HashSet<i64> = corps["local"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_i64().unwrap())
        .collect();
    let ids2: HashSet<i64> = page2["local"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids1.is_disjoint(&ids2),
        "deux pages d'albums qui se recouvrent : l'ordre n'est pas total"
    );
}

// --- 5 et 6. ce qu'un client déjà installé continue de voir ------------

/// Un client 0.9.130 lit `local.tracks` comme un tableau, et ignore ce qu'il
/// ne connaît pas. La forme qu'il lit ne bouge pas.
#[tokio::test]
async fn la_forme_que_lit_un_client_0_9_130_est_intacte() {
    let state = etat();
    bibliotheque(&state);

    let corps = chercher(&state, 30, None).await;

    assert!(corps["local"]["tracks"].is_array());
    assert!(corps["local"]["albums"].is_array());
    assert!(corps["local"]["artists"].is_array());
    assert!(
        corps.get("radios").is_some(),
        "`radios` reste au premier niveau"
    );
    assert!(
        corps.get("services").is_some(),
        "`services` reste au premier niveau — la moitié streaming n'est pas touchée"
    );
    // Et la limite reste ce que le client a demandé : elle n'est pas relevée
    // en douce. Corriger #3189 en rendant 200 lignes au lieu de 50 aurait
    // déplacé le défaut, pas levé le mensonge.
    assert_eq!(entier(&corps, &["local", "limit"]), 30);
    assert_eq!(pistes(&corps).len(), 30);
}

/// Sans `offset`, la première page est celle d'avant : même limite, mêmes
/// pistes, dans le même ordre.
#[tokio::test]
async fn sans_offset_la_premiere_page_est_celle_d_avant() {
    let state = etat();
    bibliotheque(&state);

    let sans = chercher(&state, 25, None).await;
    let avec_zero = chercher(&state, 25, Some(0)).await;

    assert_eq!(
        sans["local"]["tracks"], avec_zero["local"]["tracks"],
        "`offset` absent doit valoir `offset=0`"
    );
    assert_eq!(entier(&sans, &["local", "offset"]), 0);
}
