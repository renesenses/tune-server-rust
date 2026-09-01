//! Tri aléatoire des albums, avec bouton de re-tirage (#3074).
//!
//! Demande de Steve Taylor (forum Mozaiklabs, fil 1635) : un tri « au hasard »
//! dans la vue Bibliothèque, plus un bouton qui re-tire. Les deux moitiés se
//! règlent avec la même pièce — une GRAINE explicite portée par la requête :
//! re-tirer, c'est changer de graine.
//!
//! Ce qui décide de la forme du correctif n'est pas le hasard, c'est la
//! PAGINATION. La grille charge ses 3 357 albums en quatre requêtes
//! (`offset=0/100`, puis 0, 2000, 4000). Un `ORDER BY RANDOM()` branché
//! naïvement — la forme qu'emploient déjà `smart_collections.rs` et
//! `smart_playlists.rs`, mais eux ne posent qu'un `LIMIT`, jamais d'`OFFSET` —
//! re-tire à CHAQUE requête : la grille montrerait des albums en double et en
//! cacherait d'autres, sans rien dire.
//!
//! Les épreuves ci-dessous portent donc sur des FAITS d'ordre, jamais sur un
//! code HTTP : « deux graines, deux ordres », « la même graine, le même ordre
//! sur deux pages ». Un 200 ne prouve rien ici — avant ce correctif,
//! `sort=random` rendait 200 en retombant en silence sur l'ordre des
//! identifiants (`_ => format!("a.id {dir}")` dans `AlbumRepo::list_filtered`).
//!
//! Elles tournent contre le VRAI routeur et une VRAIE base SQLite en mémoire :
//! c'est le seul niveau où l'on prouve à la fois que `?seed=` se désérialise,
//! que le SQL s'exécute, et que la route est bien BRANCHÉE sur le mécanisme.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

/// Assez d'albums pour que « deux ordres identiques par hasard » soit
/// impossible (60! tirages), et assez pour paginer en cinq pages de douze.
const TAILLE: usize = 60;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Soixante albums dont les titres sont RANGÉS COMME LES IDENTIFIANTS
/// (`Album 01` … `Album 60`).
///
/// Ce n'est pas un détail de confort : c'est ce qui rend le défaut visible.
/// Comme l'ordre alphabétique coïncide ici avec l'ordre des identifiants, un
/// tri « aléatoire » qui retombe sur `a.id` rend exactement la liste triée —
/// et `assert_ne!(rendu, trié)` le dit.
fn bibliotheque() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for n in 1..=TAILLE {
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO albums (id, title, source) VALUES ({n}, 'Album {n:02}', 'local')"
                ),
                &[],
            )
            .expect("insertion d'album");
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, file_path, duration_ms, format) \
                     VALUES ({n}, 'piste {n}', {n}, '/music/album{n:02}.flac', 200000, 'flac')"
                ),
                &[],
            )
            .expect("insertion de piste");
    }
    tune_server::routes::router(state)
}

fn titres(body: &Value) -> Vec<String> {
    body.get("items")
        .and_then(Value::as_array)
        .expect("la liste doit rendre des items")
        .iter()
        .filter_map(|a| a.get("title").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// LE fait de base : deux graines différentes rendent deux ordres différents.
///
/// Avant #3074 cette épreuve échouait, et pas pour un détail : `sort=random`
/// n'était reconnu NULLE PART dans `AlbumRepo::list_filtered`, il tombait dans
/// le bras fourre-tout `_ => format!("a.id {dir}")`. Les deux appels rendaient
/// donc la même liste, dans l'ordre des identifiants, avec un 200 franc — un
/// « bouton de re-tirage » qui ne retire rien.
#[tokio::test]
async fn deux_graines_donnent_deux_ordres_differents_3074() {
    let app = bibliotheque();

    let (statut, un) = get(
        &app,
        "/api/v1/library/albums?sort=random&seed=1&limit=60&offset=0",
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let un = titres(&un);
    let (_, deux) = get(
        &app,
        "/api/v1/library/albums?sort=random&seed=2&limit=60&offset=0",
    )
    .await;
    let deux = titres(&deux);

    assert_eq!(un.len(), TAILLE, "la page doit rendre les soixante albums");
    assert_ne!(
        un, deux,
        "graine 1 et graine 2 rendent le MÊME ordre : la graine n'est pas lue et \
         `sort=random` retombe en silence sur l'ordre des identifiants (#3074)"
    );

    let mut contenu_un = un.clone();
    let mut contenu_deux = deux.clone();
    contenu_un.sort();
    contenu_deux.sort();
    assert_eq!(
        contenu_un, contenu_deux,
        "mélanger ne doit RIEN ajouter ni retirer : les deux tirages portent les mêmes albums"
    );
    assert_ne!(
        un, contenu_un,
        "l'ordre rendu EST l'ordre alphabétique — donc l'ordre des identifiants : \
         rien n'a été mélangé (#3074)"
    );

    // Reproductible : la même graine redonne le même ordre. Sans cela, aucune
    // pagination n'est possible.
    let (_, encore) = get(
        &app,
        "/api/v1/library/albums?sort=random&seed=1&limit=60&offset=0",
    )
    .await;
    assert_eq!(
        titres(&encore),
        un,
        "la même graine doit redonner le MÊME ordre, sinon chaque page re-tire"
    );
}

/// La même graine tient sur toute la pagination : rien en double, rien en
/// moins.
///
/// C'est la contrainte que `ORDER BY RANDOM()` ne sait pas honorer — et la
/// raison pour laquelle le précédent de `smart_collections.rs` n'était pas
/// transposable tel quel.
#[tokio::test]
async fn la_meme_graine_pagine_sans_doublon_ni_absent_3074() {
    let app = bibliotheque();

    let mut recolle: Vec<String> = Vec::new();
    for offset in (0..TAILLE).step_by(12) {
        let (statut, page) = get(
            &app,
            &format!("/api/v1/library/albums?sort=random&seed=42&limit=12&offset={offset}"),
        )
        .await;
        assert_eq!(statut, StatusCode::OK);
        let page = titres(&page);
        assert_eq!(page.len(), 12, "page à l'offset {offset}");
        recolle.extend(page);
    }

    let (_, entier) = get(
        &app,
        "/api/v1/library/albums?sort=random&seed=42&limit=60&offset=0",
    )
    .await;
    assert_eq!(
        recolle,
        titres(&entier),
        "les cinq pages recollées doivent redonner EXACTEMENT la page unique : \
         sinon chaque offset re-tire (#3074)"
    );

    let mut sans_doublon = recolle.clone();
    sans_doublon.sort();
    sans_doublon.dedup();
    assert_eq!(
        sans_doublon.len(),
        TAILLE,
        "la pagination a doublé ou perdu des albums : {} lignes pour {} albums distincts",
        recolle.len(),
        sans_doublon.len()
    );
}

/// Sans graine, le serveur en tire une ET LA REND — c'est le contrat qui
/// permet au client de paginer sans se contredire.
///
/// La rejouer doit redonner le même ordre : c'est ce qui fait du bouton de
/// re-tirage une simple demande « page 0, sans graine ».
#[tokio::test]
async fn sans_graine_le_serveur_en_tire_une_et_la_rend_3074() {
    let app = bibliotheque();

    let (statut, tirage) = get(&app, "/api/v1/library/albums?sort=random&limit=60&offset=0").await;
    assert_eq!(statut, StatusCode::OK);
    let graine = tirage
        .get("seed")
        .and_then(Value::as_i64)
        .expect("la réponse d'un tri aléatoire doit porter la graine employée (#3074)");
    let ordre = titres(&tirage);
    assert_eq!(ordre.len(), TAILLE);

    let (_, rejoue) = get(
        &app,
        &format!("/api/v1/library/albums?sort=random&seed={graine}&limit=60&offset=0"),
    )
    .await;
    assert_eq!(
        titres(&rejoue),
        ordre,
        "rejouer la graine rendue doit redonner le même ordre, sinon le client \
         ne peut pas demander la page suivante"
    );
}

/// Le bouton de re-tirage : redemander sans graine change le tirage.
#[tokio::test]
async fn le_bouton_de_retirage_change_le_tirage_3074() {
    let app = bibliotheque();

    let (_, premier) = get(&app, "/api/v1/library/albums?sort=random&limit=60&offset=0").await;
    let (_, second) = get(&app, "/api/v1/library/albums?sort=random&limit=60&offset=0").await;

    assert_ne!(
        premier.get("seed"),
        second.get("seed"),
        "deux demandes sans graine doivent tirer deux graines : sans cela le \
         « re-tirage » du fil 1635 rend toujours la même chose"
    );
    assert_ne!(
        titres(&premier),
        titres(&second),
        "deux graines différentes doivent donner deux ordres différents"
    );
}

/// TÉMOIN — vert des deux côtés du correctif.
///
/// Les cinq tris déjà livrés, et le contenu de la réponse pour un client qui
/// ignore tout de `random`, doivent être RIGOUREUSEMENT inchangés : aucune
/// clef `seed` en trop, aucun `total` déplacé, aucun ordre bousculé. C'est la
/// seule garantie qui compte pour les clients déjà en service (iOS, macOS,
/// Android, client web, UPnP).
#[tokio::test]
async fn temoin_les_autres_tris_sont_inchanges_3074() {
    let app = bibliotheque();

    let (statut, par_titre) =
        get(&app, "/api/v1/library/albums?sort=title&order=asc&limit=60").await;
    assert_eq!(statut, StatusCode::OK);
    let rendu = titres(&par_titre);
    let mut attendu = rendu.clone();
    attendu.sort();
    assert_eq!(rendu, attendu, "le tri par titre doit rester alphabétique");
    assert_eq!(
        par_titre.get("total").and_then(Value::as_i64),
        Some(TAILLE as i64)
    );
    assert!(
        par_titre.get("seed").is_none(),
        "aucune graine ne doit apparaître hors du tri aléatoire : la réponse des \
         clients déjà livrés ne bouge pas"
    );

    let (statut, defaut) = get(&app, "/api/v1/library/albums?limit=60").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(titres(&defaut).len(), TAILLE);
    assert!(defaut.get("seed").is_none());
}
