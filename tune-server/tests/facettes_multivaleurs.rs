//! Oxygen : plusieurs valeurs par facette (#2168, fil forum 1513).
//!
//! Ces épreuves tournent contre le VRAI routeur et une VRAIE base SQLite en
//! mémoire — pas contre le constructeur de SQL. C'est le seul niveau où l'on
//! peut prouver trois choses à la fois :
//!
//! 1. que `?format=aiff&format=flac` rend bien l'union des deux (OU dans une
//!    facette), et que `&genre=Jazz` la restreint (ET entre facettes) ;
//! 2. que **les effectifs affichés à côté de chaque valeur restent justes** en
//!    sélection multiple — un compteur faux est pire que pas de compteur ;
//! 3. que le SQL s'exécute réellement (un `IN (…)` mal formé ou un paramètre lié
//!    dans le désordre échoue ici, là où un test de chaîne de caractères ne
//!    verrait rien).
//!
//! La correction PostgreSQL, elle, est prouvée à part par
//! `facets::tests::les_marqueurs_et_les_valeurs_restent_alignes_sur_les_deux_moteurs`,
//! qui exige que les marqueurs `$n` forment la suite 1..N sans trou : c'est
//! exactement ce que SQLite ne peut PAS attraper, ses marqueurs étant tous `?`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

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
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

/// Une bibliothèque d'épreuve dont chaque case est connue d'avance.
///
/// | format | genre | pistes |
/// |--------|-------|--------|
/// | aiff   | Jazz  | 3      |
/// | aiff   | Rock  | 2      |
/// | flac   | Jazz  | 4      |
/// | wav    | Rock  | 5      |
/// | mp3    | Jazz  | 1      |
///
/// Total : 15 pistes. Les nombres sont tous distincts et leurs sommes aussi,
/// pour qu'aucun compte juste ne puisse l'être par accident.
fn bibliotheque() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let mut n = 0;
    for (format, genre, combien) in [
        ("aiff", "Jazz", 3),
        ("aiff", "Rock", 2),
        ("flac", "Jazz", 4),
        ("wav", "Rock", 5),
        ("mp3", "Jazz", 1),
    ] {
        for _ in 0..combien {
            n += 1;
            state
                .backend
                .execute(
                    &format!(
                        "INSERT INTO tracks (title, artist_id, file_path, duration_ms, format, genre, sample_rate) \
                         VALUES ('Piste {n}', NULL, '/music/p{n}.{format}', 200000, '{format}', '{genre}', 44100)"
                    ),
                    &[],
                )
                .expect("insertion de piste");
        }
    }
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

fn total(body: &Value) -> i64 {
    body.get("total").and_then(Value::as_i64).unwrap_or(-1)
}

/// Les effectifs rendus pour une facette, sous forme de couples (valeur, n).
fn effectifs(body: &Value, champ: &str) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = body
        .get(champ)
        .and_then(Value::as_array)
        .expect("la facette demandée doit être rendue")
        .iter()
        .map(|e| {
            (
                e.get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                e.get("count").and_then(Value::as_i64).unwrap_or(-1),
            )
        })
        .collect();
    v.sort();
    v
}

/// La demande de Cyrille : `aiff` **OU** `flac`, dans la même facette.
#[tokio::test]
async fn plusieurs_valeurs_dans_une_facette_sunissent() {
    let (app, _s) = bibliotheque();

    let (st, tout) = get(&app, "/api/v1/library/tracks?limit=100").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        total(&tout),
        15,
        "la bibliothèque d'épreuve compte 15 pistes"
    );

    let (_, aiff) = get(&app, "/api/v1/library/tracks?limit=100&format=aiff").await;
    assert_eq!(total(&aiff), 5);
    let (_, flac) = get(&app, "/api/v1/library/tracks?limit=100&format=flac").await;
    assert_eq!(total(&flac), 4);

    // ⚠️ LE point de la demande. Avant #2168 cette URL rendait 4 (le `=` ne
    // retenait que la dernière valeur vue par serde), pas 9.
    let (st, deux) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        total(&deux),
        9,
        "aiff OU flac = 5 + 4 ; obtenu {}",
        total(&deux)
    );
    assert_eq!(
        deux.get("items").and_then(Value::as_array).unwrap().len(),
        9,
        "la liste rendue doit contenir autant de pistes que le total annoncé"
    );

    // Trois valeurs, pour que le `IN (…)` ne soit pas juste par hasard à deux.
    let (_, trois) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac&format=mp3",
    )
    .await;
    assert_eq!(total(&trois), 10);
}

/// L'autre moitié de la sémantique : deux facettes différentes se combinent en
/// **ET**. Ajouter une valeur élargit, ajouter une facette restreint.
#[tokio::test]
async fn deux_facettes_differentes_se_combinent_en_et() {
    let (app, _s) = bibliotheque();

    let (_, r) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac&genre=Jazz",
    )
    .await;
    assert_eq!(
        total(&r),
        7,
        "(aiff OU flac) ET Jazz = 3 + 4 ; obtenu {}",
        total(&r)
    );

    // Le OU interne doit rester enfermé : si le SQL se lisait
    // `format = aiff OR (format = flac AND genre = Jazz)`, on obtiendrait 9.
    assert_ne!(total(&r), 9, "le OU de la facette a débordé sur le ET");

    let (_, r) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac&genre=Jazz&genre=Rock",
    )
    .await;
    assert_eq!(total(&r), 9, "(aiff OU flac) ET (Jazz OU Rock)");
}

/// **Les compteurs restent justes en sélection multiple.**
///
/// C'est la partie la plus facile à casser. La règle : en comptant la facette
/// F, on applique toutes les AUTRES facettes et jamais F elle-même. L'effectif
/// affiché à côté d'une valeur répond donc toujours à la même question, et
/// **ne bouge pas** quand on coche une deuxième valeur de la même facette —
/// ce qu'attend quiconque a déjà vu une liste de cases à cocher.
#[tokio::test]
async fn les_effectifs_restent_justes_en_selection_multiple() {
    let (app, _s) = bibliotheque();

    let attendu_sans_filtre = vec![
        ("aiff".to_string(), 5),
        ("flac".to_string(), 4),
        ("mp3".to_string(), 1),
        ("wav".to_string(), 5),
    ];

    let (st, f0) = get(&app, "/api/v1/library/facets?fields=format&limit=0").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(effectifs(&f0, "format"), attendu_sans_filtre);

    // Une valeur cochée : la facette s'exclut elle-même, ses effectifs ne
    // bougent pas (c'est ainsi qu'on peut encore en cocher une autre).
    let (_, f1) = get(
        &app,
        "/api/v1/library/facets?fields=format&limit=0&format=aiff",
    )
    .await;
    assert_eq!(effectifs(&f1, "format"), attendu_sans_filtre);

    // DEUX valeurs cochées : toujours les mêmes effectifs. Un `IN (…)` qui
    // aurait fuité dans le comptage donnerait ici aiff=5, flac=4 et RIEN
    // d'autre — les deux autres formats disparaîtraient du rail.
    let (_, f2) = get(
        &app,
        "/api/v1/library/facets?fields=format&limit=0&format=aiff&format=flac",
    )
    .await;
    assert_eq!(
        effectifs(&f2, "format"),
        attendu_sans_filtre,
        "cocher une deuxième valeur ne doit pas changer les effectifs de SA facette"
    );

    // Et l'effectif annoncé doit être TENU : cocher `wav` en plus doit ajouter
    // exactement les 5 pistes que le rail annonçait.
    let (_, avant) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac",
    )
    .await;
    let (_, apres) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac&format=wav",
    )
    .await;
    assert_eq!(
        total(&apres) - total(&avant),
        5,
        "le rail annonçait 5 pistes wav : la liste doit en gagner exactement 5"
    );
}

/// Les effectifs des AUTRES facettes, eux, doivent bien se resserrer sur la
/// sélection multiple — sinon le rail annoncerait des filtres qui ne rendent
/// pas ce qu'ils promettent.
#[tokio::test]
async fn les_autres_facettes_se_resserrent_sur_la_selection_multiple() {
    let (app, _s) = bibliotheque();

    let (_, f) = get(&app, "/api/v1/library/facets?fields=genre&limit=0").await;
    assert_eq!(
        effectifs(&f, "genre"),
        vec![("Jazz".to_string(), 8), ("Rock".to_string(), 7)]
    );

    // (aiff OU flac) : Jazz = 3 + 4 = 7, Rock = 2.
    let (_, f) = get(
        &app,
        "/api/v1/library/facets?fields=genre&limit=0&format=aiff&format=flac",
    )
    .await;
    assert_eq!(
        effectifs(&f, "genre"),
        vec![("Jazz".to_string(), 7), ("Rock".to_string(), 2)]
    );

    // Et la somme des effectifs d'une facette dont chaque piste ne porte qu'UNE
    // valeur doit égaler le total de la liste filtrée : c'est l'accord entre le
    // rail et la liste, le défaut redouté du chantier.
    let (_, liste) = get(
        &app,
        "/api/v1/library/tracks?limit=100&format=aiff&format=flac",
    )
    .await;
    let somme: i64 = effectifs(&f, "genre").iter().map(|(_, n)| n).sum();
    assert_eq!(
        somme,
        total(&liste),
        "le rail et la liste doivent compter la même chose"
    );
}

/// La vue « cartes album » passe par un autre point d'entrée
/// (`/library/albums-detailed`) qui réutilise le même constructeur : elle doit
/// voir exactement la même sélection.
#[tokio::test]
async fn les_cartes_album_voient_la_meme_selection() {
    let (app, state) = bibliotheque();
    // Les cartes n'affichent que de vrais albums ; on en donne un aux 4 flac.
    state
        .backend
        .execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Kind of Blue', NULL)",
            &[],
        )
        .expect("insertion d'album");
    let album_id = state.backend.last_insert_rowid();
    state
        .backend
        .execute(
            &format!("UPDATE tracks SET album_id = {album_id} WHERE format = 'flac'"),
            &[],
        )
        .expect("rattachement");

    let (st, cartes) = get(
        &app,
        "/api/v1/library/albums-detailed?format=aiff&format=flac",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let items = cartes.get("items").and_then(Value::as_array).unwrap();
    assert_eq!(
        items.len(),
        1,
        "un seul album a des pistes dans la sélection"
    );
    assert_eq!(
        items[0].get("track_count").and_then(Value::as_i64),
        Some(4),
        "les 4 pistes flac de l'album"
    );

    // Sélection qui n'inclut pas flac : plus aucune carte.
    let (_, cartes) = get(&app, "/api/v1/library/albums-detailed?format=wav").await;
    assert_eq!(
        cartes.get("items").and_then(Value::as_array).unwrap().len(),
        0
    );
}

/// **Rétrocompatibilité.** Une URL ou un état enregistré d'avant #2168 ne porte
/// qu'une valeur par facette : il doit continuer de rendre exactement la même
/// chose.
#[tokio::test]
async fn une_url_dune_seule_valeur_rend_toujours_la_meme_chose() {
    let (app, _s) = bibliotheque();

    for (url, attendu) in [
        ("/api/v1/library/tracks?limit=100&format=aiff", 5),
        ("/api/v1/library/tracks?limit=100&genre=Jazz", 8),
        ("/api/v1/library/tracks?limit=100&format=wav&genre=Rock", 5),
        ("/api/v1/library/tracks?limit=100&sample_rate=44100", 15),
        ("/api/v1/library/tracks?limit=100&format=aiff&genre=Rock", 2),
    ] {
        let (st, r) = get(&app, url).await;
        assert_eq!(st, StatusCode::OK, "{url}");
        assert_eq!(total(&r), attendu, "{url}");
    }
}

/// ⚠️ **Le défaut redouté**, sous ses trois formes : un filtre qui ne filtre
/// rien ne doit JAMAIS rendre la bibliothèque entière en silence.
///
/// Le troisième cas est un défaut RÉEL corrigé au passage : avant #2168,
/// `?favorite=1` comptait comme un filtre (`Option::is_some`) mais ne produisait
/// aucune condition SQL (`_ => {}`) — la route empruntait le chemin filtré,
/// n'y filtrait rien, et rendait les 15 pistes en annonçant un filtre actif.
#[tokio::test]
async fn un_filtre_qui_ne_filtre_rien_ne_rend_pas_la_bibliotheque_entiere() {
    let (app, _s) = bibliotheque();

    // 1. Une valeur qui n'existe pas : zéro piste, jamais quinze.
    let (st, r) = get(&app, "/api/v1/library/tracks?limit=100&format=dsf").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(total(&r), 0);

    // 2. Une valeur hors du vocabulaire FERMÉ des favoris. Le filtre ne peut
    //    produire aucun prédicat : la route doit alors rendre la bibliothèque
    //    parce qu'AUCUN filtre n'est actif — et surtout pas en annonçant un
    //    filtre. Ce qui compte ici est qu'elle ne rende pas 15 pistes en
    //    prétendant filtrer, mais qu'elle se comporte comme une requête nue.
    let (st, hors) = get(&app, "/api/v1/library/tracks?limit=100&favorite=1").await;
    assert_eq!(st, StatusCode::OK);
    let (_, nue) = get(&app, "/api/v1/library/tracks?limit=100").await;
    assert_eq!(total(&hors), total(&nue));

    // 3. Mais dès qu'une SEULE valeur du vocabulaire est reconnue, elle filtre :
    //    aucun favori n'a été posé, donc zéro piste.
    let (_, favori) = get(&app, "/api/v1/library/tracks?limit=100&favorite=album").await;
    assert_eq!(
        total(&favori),
        0,
        "un favori reconnu doit filtrer, pas laisser passer"
    );

    // 4. Une facette vide n'est pas un filtre : même résultat qu'une requête nue.
    let (_, vide) = get(&app, "/api/v1/library/tracks?limit=100&format=&genre=").await;
    assert_eq!(total(&vide), 15);
}

/// Défaut PRÉEXISTANT trouvé en réécrivant `list_filtered`, corrigé ici parce
/// qu'il vit dans la fonction réécrite : la recherche libre écrivait DEUX fois
/// le même marqueur pour UNE seule valeur liée. Légal en PostgreSQL (`$1`
/// répété), fatal en SQLite où chaque `?` anonyme consomme un indice —
/// `rusqlite` refusait le compte et `GET /library/tracks?q=…` rendait une liste
/// VIDE avec un total à zéro, sur l'installation par défaut.
#[tokio::test]
async fn la_recherche_libre_fonctionne_sur_sqlite() {
    let (app, state) = bibliotheque();
    state
        .backend
        .execute("INSERT INTO artists (name) VALUES ('Miles Davis')", &[])
        .expect("insertion d'artiste");
    let artiste = state.backend.last_insert_rowid();
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO tracks (title, artist_id, file_path, duration_ms, format, genre) \
                 VALUES ('So What', {artiste}, '/music/so_what.flac', 200000, 'flac', 'Jazz')"
            ),
            &[],
        )
        .expect("insertion");

    // Par le TITRE.
    let (st, r) = get(&app, "/api/v1/library/tracks?limit=100&q=so%20what").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(total(&r), 1, "la recherche par titre ne doit pas rendre 0");

    // Par le NOM D'ARTISTE — c'est la moitié de la requête que le second
    // marqueur portait, donc celle que le défaut faisait tomber.
    let (_, r) = get(&app, "/api/v1/library/tracks?limit=100&q=miles").await;
    assert_eq!(
        total(&r),
        1,
        "la recherche par artiste ne doit pas rendre 0"
    );

    // Et elle se combine en ET avec une facette multivaluée.
    let (_, r) = get(
        &app,
        "/api/v1/library/tracks?limit=100&q=miles&format=flac&format=aiff",
    )
    .await;
    assert_eq!(total(&r), 1);
    let (_, r) = get(&app, "/api/v1/library/tracks?limit=100&q=miles&format=wav").await;
    assert_eq!(total(&r), 0);
}

/// La virgule dans une valeur : la raison pour laquelle le format retenu est la
/// clé RÉPÉTÉE et non une liste séparée par des virgules. Un genre « Jazz, Blues »
/// doit rester UNE valeur, et continuer de filtrer.
#[tokio::test]
async fn une_valeur_a_virgule_reste_filtrable() {
    let (app, state) = bibliotheque();
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, artist_id, file_path, duration_ms, format, genre) \
             VALUES ('Piste 16', NULL, '/music/p16.flac', 200000, 'flac', 'Jazz, Blues')",
            &[],
        )
        .expect("insertion");

    let (st, r) = get(
        &app,
        "/api/v1/library/tracks?limit=100&genre=Jazz%2C%20Blues",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        total(&r),
        1,
        "la valeur ne doit pas avoir été coupée en deux"
    );

    // Et en sélection multiple avec une valeur ordinaire.
    let (_, r) = get(
        &app,
        "/api/v1/library/tracks?limit=100&genre=Jazz%2C%20Blues&genre=Rock",
    )
    .await;
    assert_eq!(total(&r), 8, "1 + 7");
}
