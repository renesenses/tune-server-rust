//! #5526 — « Aléatoire » sur 25 albums : ≈ 30 s avant la première note chez
//! Sevy Tabroc (fil 2051, 0.9.168, macOS, SQLite, 109 004 pistes).
//!
//! Le journal montrait six `slow_query` de ≈ 560 ms en série, toutes
//! `SELECT t.id, COUNT(*) OVER () FROM tracks t …` — la première passe de
//! [`TrackRepo::list_visible_avec_total`], donc `GET /library/tracks` sans
//! facette. Leur appelant est le client web (`tune-web-client`,
//! `LibraryV2.svelte`, branche `albumsAleatoire` de `shuffleAll`) : pour tirer
//! dans 25 albums, il chargeait TOUTE la bibliothèque par `api.getAllTracks()`,
//! pages de 2 000, l'une après l'autre — une cinquantaine sur 109 004 pistes.
//! Six seulement dépassaient le seuil `slow_query` (500 ms) : celles des plus
//! grands décalages. Les autres, juste en dessous, ne laissaient aucune trace.
//!
//! La correction est côté client : une sélection de quelques albums se lit
//! fiche par fiche (`GET /library/albums/{id}/tracks`, cinq requêtes à la
//! fois, `api.getAlbumTracksBatch`). Ce banc rejoue les DEUX suites de
//! requêtes du client contre le vrai routeur, sur une bibliothèque au profil
//! de celle de Sevy, et mesure le temps entre la demande d'aléatoire et le
//! moment où le client peut envoyer son `POST /playback/play` — le lancement
//! lui-même (`playback_timing … total_ms=676` chez Sevy) est le même dans les
//! deux cas, il n'entre pas dans la mesure.
//!
//! * `i5526_la_fiche_par_album_rend_les_memes_pistes_que_la_bibliotheque` :
//!   toujours joué. Les deux chemins retiennent le MÊME ensemble de pistes —
//!   copies de moindre qualité repliées comprises —, c'est ce sur quoi la
//!   correction du client s'appuie.
//! * `i5526_banc_aleatoire_25_albums_109k_pistes` : `#[ignore]`, le banc
//!   chronométré (≈ 109 500 pistes). À lancer à la main :
//!   `cargo test -p tune-server --release i5526_banc -- --ignored --nocapture`.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use tower::ServiceExt;

type Etat = crate::state::AppState;

/// Taille d'une page de `api.getAllTracks()` (client web, `pageSize = 2000`).
const PAGE_CLIENT: usize = 2_000;
/// Concurrence de `api.getAlbumTracksBatch` (client web).
const CONCURRENCE_CLIENT: usize = 5;

fn demarrer(chemin: &str) -> axum::Router {
    let state = Etat::new(chemin, 0, Default::default()).expect("démarrage");
    crate::routes::router(state)
}

/// Une bibliothèque de `albums` albums, 12 ou 13 pistes FLAC chacun (12,5 en
/// moyenne : 8 762 albums → ≈ 109 500 pistes, le profil de Sevy), `albums / 3`
/// artistes. Un album sur 40 porte en plus une copie MP3 de chaque piste :
/// la liste des pistes comme la fiche doivent la replier (#1362, #4101).
///
/// Le schéma vient d'un premier démarrage ; la base est remplie en UNE
/// transaction, puis le serveur est redémarré sur elle — les passes de
/// démarrage (index de la clé de copie, #5138) voient des données.
fn remplir(chemin: &str, albums: i64) {
    drop(demarrer(chemin));
    let artistes = (albums / 3).max(10);
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=artistes {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    let mut id = 0_i64;
    for al in 1..=albums {
        let artiste = al % artistes + 1;
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, 'Album {al}', {artiste}, 'local');\n"
        ));
        let n_pistes = if al % 2 == 0 { 12 } else { 13 };
        let formats: &[(&str, i64)] = if al % 40 == 1 {
            &[("flac", 16), ("mp3", 16)]
        } else {
            &[("flac", 16)]
        };
        for (format, bits) in formats {
            for n in 1..=n_pistes {
                id += 1;
                sql.push_str(&format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
                     file_path, format, sample_rate, bit_depth, duration_ms, source, album_artist) \
                     VALUES ({id}, 'Piste {n}', {al}, {artiste}, 1, {n}, '/banc/{id}.{format}', \
                     '{format}', 44100, {bits}, 240000, 'local', '');\n"
                ));
            }
        }
    }
    sql.push_str("COMMIT;\n");
    let conn = rusqlite::Connection::open(chemin).expect("ouverture directe du banc");
    conn.execute_batch(&sql).expect("remplissage du banc");
}

async fn get_json(app: &axum::Router, uri: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        reponse.status().is_success(),
        "{uri} : {}",
        reponse.status()
    );
    let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&corps).unwrap()
}

/// `(id, album_id)` des pistes d'une réponse (tableau nu ou `{items}`).
fn pistes_de(json: &Value) -> Vec<(i64, Option<i64>)> {
    json.as_array()
        .or_else(|| json["items"].as_array())
        .unwrap_or_else(|| panic!("liste de pistes attendue : {json}"))
        .iter()
        .map(|t| (t["id"].as_i64().expect("id"), t["album_id"].as_i64()))
        .collect()
}

/// Ce que le client retient : les pistes des albums choisis, triées par id
/// (le mélange et le plafond viennent après, identiques des deux côtés).
fn retenues(pistes: &[(i64, Option<i64>)], albums: &[i64]) -> BTreeSet<i64> {
    pistes
        .iter()
        .filter(|(_, al)| al.is_some_and(|a| albums.contains(&a)))
        .map(|(id, _)| *id)
        .collect()
}

/// AVANT : `api.getAllTracks()` — toute la bibliothèque, page après page.
async fn avant(app: &axum::Router, albums: &[i64]) -> (BTreeSet<i64>, usize) {
    let mut toutes = Vec::new();
    let mut requetes = 0;
    let mut offset = 0;
    loop {
        let page = pistes_de(
            &get_json(
                app,
                &format!("/api/v1/library/tracks?limit={PAGE_CLIENT}&offset={offset}"),
            )
            .await,
        );
        requetes += 1;
        let n = page.len();
        toutes.extend(page);
        if n < PAGE_CLIENT {
            break;
        }
        offset += PAGE_CLIENT;
    }
    (retenues(&toutes, albums), requetes)
}

/// APRÈS : `api.getAlbumTracksBatch(ids)` — la fiche de chaque album, cinq à
/// la fois.
async fn apres(app: &axum::Router, albums: &[i64]) -> (BTreeSet<i64>, usize) {
    let pages: Vec<Vec<(i64, Option<i64>)>> = stream::iter(albums.iter().copied())
        .map(|al| async move {
            pistes_de(&get_json(app, &format!("/api/v1/library/albums/{al}/tracks")).await)
        })
        .buffer_unordered(CONCURRENCE_CLIENT)
        .collect()
        .await;
    let toutes: Vec<_> = pages.into_iter().flatten().collect();
    (retenues(&toutes, albums), albums.len())
}

/// 25 albums répartis dans la bibliothèque, dont des albums à copie MP3.
fn selection(albums: i64) -> Vec<i64> {
    let pas = albums / 25;
    (0..25).map(|k| 1 + k * pas).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn i5526_la_fiche_par_album_rend_les_memes_pistes_que_la_bibliotheque() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let chemin = chemin.to_str().unwrap();
    remplir(chemin, 400);
    let app = demarrer(chemin);
    let choix = selection(400);
    assert!(
        choix.iter().any(|al| al % 40 == 1),
        "la sélection doit contenir un album à copie MP3"
    );

    let (par_bibliotheque, pages) = avant(&app, &choix).await;
    let (par_fiche, fiches) = apres(&app, &choix).await;

    assert_eq!(
        par_fiche, par_bibliotheque,
        "#5526 — la fiche par album doit rendre EXACTEMENT les pistes que la \
         bibliothèque entière retient pour ces albums (copies repliées comprises)"
    );
    // 13 ou 12 pistes par album, copies MP3 repliées : aucune piste en trop.
    let attendu: usize = choix
        .iter()
        .map(|al| if al % 2 == 0 { 12 } else { 13 })
        .sum();
    assert_eq!(par_fiche.len(), attendu, "copies MP3 non repliées ?");
    assert_eq!(fiches, 25);
    assert!(
        pages >= 3,
        "400 albums doivent tenir sur plusieurs pages ({pages})"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "banc chronométré, ≈ 109 500 pistes — lancer avec --ignored --nocapture"]
async fn i5526_banc_aleatoire_25_albums_109k_pistes() {
    const ALBUMS: i64 = 8_762;
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let chemin = chemin.to_str().unwrap();
    let t = Instant::now();
    remplir(chemin, ALBUMS);
    let app = demarrer(chemin);
    eprintln!("banc #5526 : {ALBUMS} albums remplis en {:?}", t.elapsed());
    let choix = selection(ALBUMS);

    // Un tour à blanc de chaque chemin : cache de pages SQLite chaud des deux
    // côtés, pour ne pas mesurer le premier accès au fichier.
    let _ = apres(&app, &choix).await;
    let _ = get_json(&app, "/api/v1/library/tracks?limit=1&offset=0").await;

    let t = Instant::now();
    let (par_bibliotheque, pages) = avant(&app, &choix).await;
    let d_avant = t.elapsed();
    let t = Instant::now();
    let (par_fiche, fiches) = apres(&app, &choix).await;
    let d_apres = t.elapsed();

    eprintln!(
        "banc #5526 — demande d'aléatoire (25 albums) → POST /playback/play prêt :\n  \
         AVANT (getAllTracks)       : {d_avant:>10.3?}  {pages} requêtes /library/tracks, {} pistes retenues\n  \
         APRÈS (getAlbumTracksBatch): {d_apres:>10.3?}  {fiches} requêtes /library/albums/{{id}}/tracks, {} pistes retenues",
        par_bibliotheque.len(),
        par_fiche.len(),
    );
    assert_eq!(
        par_fiche, par_bibliotheque,
        "les deux chemins doivent retenir les mêmes pistes"
    );
    assert!(
        d_apres * 10 < d_avant,
        "#5526 — la fiche par album doit être au moins dix fois plus rapide : \
         {d_apres:?} contre {d_avant:?}"
    );
    assert!(
        d_apres < Duration::from_secs(1),
        "#5526 — 25 fiches d'album doivent tenir sous la seconde : {d_apres:?}"
    );
}
