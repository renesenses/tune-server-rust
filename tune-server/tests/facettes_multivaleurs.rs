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

/// Une bibliothèque minuscule mais complète : chaque famille de facette a au
/// moins une valeur, et une piste sans étiquettes alimente `untagged`.
///
/// Les identifiants sont explicites pour que les collections, favoris et
/// listes de lecture puissent viser les mêmes lignes sans dépendre de l'ordre
/// d'insertion ni du contenu semé par les migrations.
fn bibliotheque_contrat_facettes() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (93001, 'ArtisteA')",
        "INSERT INTO artists (id, name) VALUES (93002, 'ArtisteB')",
        "INSERT INTO albums (id, title, artist_id, original_year, cover_path) \
         VALUES (92001, 'AlbumA', 93001, 1959, '/covers/a.jpg')",
        "INSERT INTO albums (id, title, artist_id, original_year, cover_path) \
         VALUES (92002, 'AlbumB', 93002, 1960, NULL)",
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, composer, year, label) VALUES \
         (91001, 'PisteA', 92001, 93001, '/contrat/a.flac', 1000, 'flac', 96000, \
          24, 'local', 'Jazz', 'Bach', 1959, 'ECM')",
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, composer, year, label) VALUES \
         (91002, 'PisteB', 92002, 93002, '/contrat/b.flac', 1000, 'flac', 44100, \
          16, 'qobuz', 'Rock', 'Ravel', 1960, 'BlueNote')",
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, composer, year, label) VALUES \
         (91003, 'PisteC', 92001, 93001, '/contrat/c.aiff', 1000, 'aiff', 48000, \
          24, 'local', 'Jazz', 'Bach', 1959, 'ECM')",
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, composer, year, label) VALUES \
         (91004, 'SansTags', NULL, NULL, '/contrat/sans-tags.flac', 1000, 'flac', \
          44100, 16, 'local', NULL, NULL, NULL, NULL)",
        // Casse DIFFÉRENTE de « Jazz »/« flac » : sans elle, désaccorder
        // `in_list_ci` en `in_list` d'un seul côté ne changeait RIEN et le
        // garde-fou restait vert (#1864).
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, composer, year, label) VALUES \
         (91005, 'PisteD', 92001, 93001, '/contrat/d.flac', 1000, 'FLAC', 96000, \
          24, 'local', 'JAZZ', 'Bach', 1959, 'ECM')",
        // Genre MULTIVALUÉ dans la colonne JSON `genres`, et volontairement
        // « Jazzy » : le motif juste (`%\"Jazz\"%`) ne le prend pas, un motif
        // relâché (`%Jazz%`) le prendrait — c'est ce qui rend la divergence
        // détectable.
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, genres, composer, year, label) VALUES \
         (91006, 'PisteE', NULL, NULL, '/contrat/e.aiff', 1000, 'aiff', 48000, \
          24, 'local', NULL, '[\"Jazzy\"]', NULL, NULL, NULL)",
        // #1821 — un disque dont le tag porte PLUSIEURS genres. C'est ce
        // qu'écrivent nativement Vorbis Comment (champ `GENRE` répété) et MP4
        // (atome `©gen` répété) : la colonne `genre` garde le principal, le
        // tableau `genres` les garde tous. Le rail ne comptait que la colonne,
        // donc « Fusion » n'apparaissait nulle part — alors que le filtre, lui,
        // savait le trouver.
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, genres, composer, year, label) VALUES \
         (91008, 'PisteF', 92001, 93001, '/contrat/g.flac', 1000, 'flac', 96000, \
          24, 'local', 'Jazz', '[\"Jazz\",\"Fusion\"]', 'Bach', 1959, 'ECM')",
        // Même genre secondaire, CASSE différente, et seulement dans le
        // tableau JSON : sans `LOWER()` des deux côtés du `LIKE`, cette ligne
        // est trouvée sur SQLite et perdue sur PostgreSQL.
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, genres, composer, year, label) VALUES \
         (91009, 'PisteG', NULL, NULL, '/contrat/h.flac', 1000, 'flac', 44100, \
          16, 'local', NULL, '[\"FUSION\"]', NULL, NULL, NULL)",
        // Titre ACCENTUÉ : la liste passe par `unaccent()`, le compteur doit
        // en faire autant, sinon `q=cafe` compte sans cette piste.
        "INSERT INTO tracks \
         (id, title, album_id, artist_id, file_path, duration_ms, format, sample_rate, \
          bit_depth, source, genre, composer, year, label) VALUES \
         (91007, 'Café Bleu', 92002, 93002, '/contrat/f.wav', 1000, 'wav', 44100, \
          16, 'local', 'Rock', 'Ravel', 1960, 'BlueNote')",
        // Collection INTELLIGENTE : elle ne résout AUCUN album, seulement des
        // pistes. Le compteur qui n'en connaissait que les collections
        // manuelles posait `1 = 0` et vidait tout le rail (#1864).
        "INSERT INTO smart_collections (name, rules, match_mode) VALUES \
         ('CollectionSmart', '[{\"field\":\"artist_name\",\"op\":\"=\",\"value\":\"ArtisteB\"}]', 'all')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91001, 'release_country', 'FR')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91001, 'mood', 'Calme')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91001, 'source_media', 'CD')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91002, 'release_country', 'US')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91002, 'mood', 'Intense')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91002, 'source_media', 'SACD')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91003, 'release_country', 'FR')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91003, 'mood', 'Calme')",
        "INSERT INTO track_metadata (track_id, key, value) VALUES (91003, 'source_media', 'Vinyle')",
        "INSERT OR IGNORE INTO profiles (id, username) VALUES (1, 'contrat-facettes')",
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (92001, 1, 5)",
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (92002, 1, 4)",
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', 91001)",
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', 92002)",
        "INSERT INTO playlists (id, name, profile_id) VALUES (94001, 'ListeA', 1)",
        "INSERT INTO playlists (id, name, profile_id) VALUES (94002, 'ListeB', 1)",
        "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (94001, 91001, 0)",
        "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (94001, 91004, 1)",
        "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (94002, 91002, 0)",
    ] {
        state.backend.execute(sql, &[]).expect(sql);
    }
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            "collections",
            r#"[{"name":"CollectionA","album_ids":[92001]},{"name":"CollectionB","album_ids":[92002]}]"#,
        )
        .expect("collections d'épreuve");
    tune_server::routes::router(state)
}

/// #1864 : pour chaque valeur annoncée par le rail, cocher cette valeur doit
/// rendre exactement le nombre de pistes promis par le rail.
///
/// Ce test traverse les deux constructeurs SQL réels — `build_conditions`
/// pour les effectifs et `TrackRepo::list_filtered` pour les pistes — au lieu
/// de comparer leurs chaînes. Une divergence de colonne, de jointure ou de
/// paramètre rougit donc sur la facette précise qui a dérivé.
#[tokio::test]
async fn chaque_effectif_de_facette_est_tenu_par_la_liste_filtree() {
    let app = bibliotheque_contrat_facettes();

    for (champ, cle) in FACETTES {
        // La facette comptée s'auto-exclut. Un filtre croisé la force à
        // emprunter aussi le chemin cumulatif ; pour `format`, on croise par
        // genre afin de ne pas filtrer la facette avant son propre comptage.
        let croise = if champ == "format" {
            "genre=Jazz"
        } else {
            "format=flac"
        };
        let chemin = format!("/api/v1/library/facets?fields={champ}&limit=0&{croise}");
        let (status, facettes) = get(&app, &chemin).await;
        assert_eq!(status, StatusCode::OK, "facette {champ}");
        let valeurs = facettes
            .get(champ)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("facette {champ} absente : {facettes}"));
        assert!(
            !valeurs.is_empty(),
            "la fixture doit exercer la facette {champ}"
        );

        for entree in valeurs {
            let valeur = entree
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("valeur absente pour {champ}: {entree}"));
            let annonce = entree.get("count").and_then(Value::as_i64).unwrap_or(-1);
            let valeur = urlencoding::encode(valeur);
            let chemin = format!("/api/v1/library/tracks?limit=1000&{croise}&{cle}={valeur}");
            let (status, pistes) = get(&app, &chemin).await;
            assert_eq!(status, StatusCode::OK, "{champ}={valeur}");
            assert_eq!(
                total(&pistes),
                annonce,
                "la facette {champ}={valeur} annonce {annonce}, mais sa liste rend {}",
                total(&pistes)
            );
        }
    }
}

/// Les 17 facettes du rail, avec la clé de filtre qui leur correspond dans la
/// chaîne de requête. `source` est le nom public de la métadonnée
/// `source_media` ; la clé de filtre reste explicitement `source_media`.
const FACETTES: [(&str, &str); 17] = [
    ("genre", "genre"),
    ("label", "label"),
    ("composer", "composer"),
    ("year", "year"),
    ("artist", "artist"),
    ("format", "format"),
    ("sample_rate", "sample_rate"),
    ("bit_depth", "bit_depth"),
    ("country", "country"),
    ("mood", "mood"),
    ("source", "source_media"),
    ("rating", "rating"),
    ("collection", "collection"),
    ("original_year", "original_year"),
    ("favorite", "favorite"),
    ("playlist", "playlist"),
    ("untagged", "untagged"),
];

/// Somme des effectifs annoncés pour une facette MONOVALUÉE et toujours
/// renseignée : elle vaut donc le nombre de pistes du jeu courant.
fn somme_effectifs(rail: &Value, champ: &str) -> i64 {
    rail.get(champ)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("facette témoin {champ} absente : {rail}"))
        .iter()
        .map(|e| {
            e.get("count")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| panic!("effectif absent : {e}"))
        })
        .sum()
}

/// #1864 — le garde-fou qui MORD.
///
/// `chaque_effectif_de_facette_est_tenu_par_la_liste_filtree` ne croisait
/// qu'UNE facette (`format`, ou `genre` quand c'est `format` qu'on compte), et
/// la facette comptée s'auto-exclut : les QUINZE autres prédicats du compteur
/// n'étaient donc jamais construits pendant le test. Contre-épreuve mesurée le
/// 30/08 : trois désaccords posés sur le seul compteur — colonne
/// (`t.sample_rate` → `t.bit_depth`), casse (`in_list_ci` → `in_list`), motif
/// multivalué (`%"g"%` → `%g%`) — le laissaient VERT tous les trois.
///
/// Ici, chaque facette est mise en position de FILTRE, et l'on compare le rail
/// qu'elle produit à la liste qu'elle rend. Le témoin est une facette
/// monovaluée et toujours renseignée, dont la somme des effectifs vaut donc
/// exactement le nombre de pistes retenues : désaccorder un prédicat d'un seul
/// côté déplace l'une des deux mesures et pas l'autre.
#[tokio::test]
async fn filtrer_par_une_facette_narrow_le_rail_comme_la_liste() {
    let app = bibliotheque_contrat_facettes();

    for (champ, cle) in FACETTES {
        // Le témoin ne doit pas être la facette qu'on met en position de
        // filtre : elle s'auto-exclurait et le prédicat testé disparaîtrait.
        let temoin = if champ == "format" {
            "bit_depth"
        } else {
            "format"
        };
        let (status, facettes) = get(
            &app,
            &format!("/api/v1/library/facets?fields={champ}&limit=0"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "facette {champ}");
        let valeurs = facettes
            .get(champ)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("facette {champ} absente : {facettes}"));
        assert!(
            !valeurs.is_empty(),
            "la fixture doit exercer la facette {champ}"
        );

        for entree in valeurs {
            let valeur = entree
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("valeur absente pour {champ}: {entree}"));
            let encodee = urlencoding::encode(valeur);

            let (status, rail) = get(
                &app,
                &format!("/api/v1/library/facets?fields={temoin}&limit=0&{cle}={encodee}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "rail sous {cle}={valeur}");
            let compte = somme_effectifs(&rail, temoin);

            let (status, pistes) = get(
                &app,
                &format!("/api/v1/library/tracks?limit=1000&{cle}={encodee}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "liste sous {cle}={valeur}");
            let rendu = total(&pistes);

            assert_eq!(
                compte, rendu,
                "sous {cle}={valeur}, le rail « {temoin} » totalise {compte} \
                 mais la liste rend {rendu}"
            );
            assert!(
                rendu > 0,
                "la fixture doit rendre au moins une piste sous {cle}={valeur}"
            );
        }
    }
}

/// #1864 — la recherche libre `q` narrow AUSSI les effectifs du rail, et elle
/// s'écrit elle aussi deux fois.
///
/// Le compteur l'écrivait sans `unaccent()` là où `TrackRepo::list_filtered`
/// l'écrivait avec : `q=cafe` comptait sans « Café Bleu », mais la liste le
/// rendait. Aucune facette n'exerçait ce prédicat, donc rien ne le tenait.
#[tokio::test]
async fn la_recherche_libre_narrow_le_rail_comme_la_liste() {
    let app = bibliotheque_contrat_facettes();

    for recherche in ["cafe", "café", "CAFE", "Bleu"] {
        let encodee = urlencoding::encode(recherche);
        let (status, rail) = get(
            &app,
            &format!("/api/v1/library/facets?fields=format&limit=0&q={encodee}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "rail q={recherche}");
        let compte = somme_effectifs(&rail, "format");

        let (status, pistes) = get(
            &app,
            &format!("/api/v1/library/tracks?limit=1000&q={encodee}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "liste q={recherche}");
        let rendu = total(&pistes);

        assert_eq!(
            compte, rendu,
            "sous q={recherche}, le rail « format » totalise {compte} \
             mais la liste rend {rendu}"
        );
        assert!(
            rendu > 0,
            "« Café Bleu » doit répondre à q={recherche} des DEUX côtés"
        );
    }
}

/// #1821 — le rail « Genre » d'Oxygen doit annoncer les genres SECONDAIRES.
///
/// DEvir, ambassadeur : « songs purchased from different platforms or labels
/// end up being categorized under different genres ». La cause mesurée est
/// l'encodage du tag, pas le vocabulaire des marchands : « ce disque est du
/// Jazz ET de la Fusion » s'écrit soit en plusieurs valeurs (Vorbis, MP4,
/// ID3v2.4), soit en une chaîne séparée (ID3v2.3), et Tune ne rangeait le
/// disque sous ses deux genres que dans le second cas.
///
/// Côté serveur, le rail groupait sur la seule colonne `t.genre` tandis que son
/// filtre jumeau testait AUSSI le tableau `t.genres` : « Fusion » n'était donc
/// proposé par aucune carte, alors que le cocher aurait bien rendu des pistes.
#[tokio::test]
async fn le_rail_genre_annonce_les_genres_secondaires() {
    let app = bibliotheque_contrat_facettes();

    let (status, facettes) = get(&app, "/api/v1/library/facets?fields=genre&limit=0").await;
    assert_eq!(status, StatusCode::OK);
    let rail = effectifs(&facettes, "genre");

    // « Fusion » n'est le genre PRINCIPAL d'aucune piste : il ne vit que dans
    // le tableau multivalué. Il doit malgré tout être proposé.
    let fusion = rail
        .iter()
        .find(|(v, _)| v.eq_ignore_ascii_case("Fusion"))
        .unwrap_or_else(|| panic!("« Fusion » absent du rail : {rail:?}"));

    // Les DEUX pistes qui le portent, quelle que soit la casse écrite dans le
    // tableau : c'est la même intention, gravée par deux logiciels différents.
    assert_eq!(
        fusion.1, 2,
        "« Fusion » doit compter les deux gravures : {rail:?}"
    );

    // Et le compteur ne ment pas : cocher la carte rend exactement ce nombre.
    let encodee = urlencoding::encode(&fusion.0);
    let (status, pistes) = get(
        &app,
        &format!("/api/v1/library/tracks?limit=1000&genre={encodee}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        total(&pistes),
        fusion.1,
        "le rail annonce {} pour « {} », la liste en rend {}",
        fusion.1,
        fusion.0,
        total(&pistes)
    );

    // La piste 91008 porte « Jazz » ET « Fusion » : elle compte une fois dans
    // chaque carte, jamais deux fois dans la même.
    let jazz = rail
        .iter()
        .find(|(v, _)| v.eq_ignore_ascii_case("Jazz"))
        .unwrap_or_else(|| panic!("« Jazz » absent du rail : {rail:?}"));
    let encodee = urlencoding::encode(&jazz.0);
    let (_, pistes) = get(
        &app,
        &format!("/api/v1/library/tracks?limit=1000&genre={encodee}"),
    )
    .await;
    assert_eq!(
        total(&pistes),
        jazz.1,
        "« Jazz » : rail {} contre liste {}",
        jazz.1,
        total(&pistes)
    );
}
