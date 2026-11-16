//! #7768 — « joue avec » (`played_with`) de
//! `GET /library/artists/{id}/albums?sections=1`, contrat § 3.3
//! (`coordination/seb-lot2/contrat-route-artiste.md`), par le VRAI routeur
//! sur une base SQLite en mémoire.
//!
//! Témoins de l'issue :
//! - un compositeur partagé ne fait pas « joue avec » (ni un producteur) ;
//! - « Various Artists » n'y apparaît jamais ;
//! - l'artiste lui-même n'y apparaît pas (fiche, nom replié, MBID) ;
//! - l'ordre suit le nombre de pistes partagées (puis le nom sans casse) ;
//! - un doublon de source (#7766) compte une fois, un album masqué pas du
//!   tout, un remixeur est audible.
//!
//! Les deux exemples complets du contrat sont comparés dans
//! `page_artiste_sections_7767.rs`.
//!
//! Cible `[[test]]` propre (`autotests = false`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
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
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

fn artiste(s: &AppState, id: i64, nom: &str) {
    exec(
        s,
        &format!("INSERT INTO artists (id, name) VALUES ({id}, '{nom}')"),
    );
}

fn album(s: &AppState, id: i64, artiste: i64, compilation: bool) {
    exec(
        s,
        &format!(
            "INSERT INTO albums (id, title, artist_id, year, source, release_type, is_compilation) \
             VALUES ({id}, 'Album {id}', {artiste}, 2020, 'local', 'album', {})",
            i32::from(compilation)
        ),
    );
}

/// Pistes `album*10 + 1 ..= album*10 + n`, à l'artiste donné.
fn pistes(s: &AppState, album: i64, n: i64, artiste: i64) {
    for i in 1..=n {
        let id = album * 10 + i;
        exec(
            s,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                 VALUES ({id}, 'Piste {id}', {album}, {artiste}, '/m/{id}.flac', 200000, 'flac', 1, {i})"
            ),
        );
    }
}

fn credit(
    s: &AppState,
    piste: i64,
    artiste: Option<i64>,
    nom: &str,
    mbid: Option<&str>,
    role: &str,
    source: &str,
) {
    let artiste = artiste.map_or("NULL".to_string(), |a| a.to_string());
    let mbid = mbid.map_or("NULL".to_string(), |m| format!("'{m}'"));
    exec(
        s,
        &format!(
            "INSERT INTO track_credits (track_id, artist_id, artist_name, artist_mbid, role, position, source) \
             VALUES ({piste}, {artiste}, '{nom}', {mbid}, '{role}', 0, '{source}')"
        ),
    );
}

const ILSE: i64 = 1;

fn bibliotheque() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    artiste(&s, ILSE, "Ilse Brandt");
    exec(
        &s,
        "UPDATE artists SET musicbrainz_id = 'mbid-ilse' WHERE id = 1",
    );
    artiste(&s, 2, "Nora Vidal");
    artiste(&s, 3, "Various Artists");
    artiste(&s, 4, "Lena Park");
    artiste(&s, 5, "Ruth Adler");
    artiste(&s, 6, "DJ Okapi");
    artiste(&s, 7, "Hidden Friend");
    artiste(&s, 8, "Seul");
    // Ana Bell n'est créditée que SANS fiche : sa fiche (autre casse) est
    // retrouvée en base.
    artiste(&s, 9, "ana bell");

    // Album 100 de Nora Vidal : Ilse à la contrebasse sur les 4 pistes.
    album(&s, 100, 2, false);
    pistes(&s, 100, 4, 2);
    for p in 1001..=1004 {
        credit(
            &s,
            p,
            Some(ILSE),
            "Ilse Brandt",
            None,
            "performer",
            "musicbrainz",
        );
        // Compositrice et productrice communes : ne comptent pas.
        credit(&s, p, Some(4), "Lena Park", None, "composer", "musicbrainz");
        credit(
            &s,
            p,
            Some(5),
            "Ruth Adler",
            None,
            "producer",
            "musicbrainz",
        );
    }
    // Un batteur sans fiche sur deux pistes, dont une en double (#7766).
    credit(
        &s,
        1001,
        None,
        "Tomas Reyes",
        None,
        "performer",
        "musicbrainz",
    );
    credit(&s, 1001, None, "Tomas Reyes", None, "performer", "balises");
    credit(
        &s,
        1002,
        None,
        "Tomas Reyes",
        None,
        "performer",
        "musicbrainz",
    );
    // Elle-même, sans fiche : par nom replié, puis par MBID sous un autre nom.
    credit(&s, 1003, None, "ilse  BRANDT", None, "vocal", "balises");
    credit(
        &s,
        1004,
        None,
        "I. Brandt",
        Some("mbid-ilse"),
        "performer",
        "balises",
    );

    // Compilation : l'artiste des pistes est « Various Artists ».
    album(&s, 200, 3, true);
    pistes(&s, 200, 2, 3);
    for p in [2001, 2002] {
        credit(
            &s,
            p,
            Some(ILSE),
            "Ilse Brandt",
            None,
            "performer",
            "musicbrainz",
        );
        credit(
            &s,
            p,
            None,
            "Various Artists",
            None,
            "artist",
            "musicbrainz",
        );
    }

    // Remixeuse d'une piste de DJ Okapi, avec une chanteuse sans fiche.
    album(&s, 300, 6, false);
    pistes(&s, 300, 1, 6);
    credit(
        &s,
        3001,
        Some(ILSE),
        "Ilse Brandt",
        None,
        "remixer",
        "musicbrainz",
    );
    credit(&s, 3001, None, "Ana Bell", None, "vocal", "musicbrainz");

    // Lena Park : Ilse n'y fait qu'écrire — pas une piste audible.
    album(&s, 400, 4, false);
    pistes(&s, 400, 3, 4);
    for p in 4001..=4003 {
        credit(
            &s,
            p,
            Some(ILSE),
            "Ilse Brandt",
            None,
            "composer",
            "musicbrainz",
        );
    }

    // Album masqué : ne compte pas.
    album(&s, 500, 7, false);
    pistes(&s, 500, 2, 7);
    credit(
        &s,
        5001,
        Some(ILSE),
        "Ilse Brandt",
        None,
        "performer",
        "musicbrainz",
    );
    exec(
        &s,
        "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 500)",
    );

    // Seul : ses propres pistes, personne avec lui.
    album(&s, 600, 8, false);
    pistes(&s, 600, 2, 8);
    s
}

async fn joue_avec(s: AppState, id: i64) -> Value {
    let app = tune_server::routes::router(s);
    let (status, body) = get(
        &app,
        &format!("/api/v1/library/artists/{id}/albums?sections=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body
}

fn noms(v: &Value) -> Vec<String> {
    v["played_with"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|j| j["artist_name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn joue_avec_rend_les_audibles_ponderes_dans_l_ordre_7768() {
    let body = joue_avec(bibliotheque(), ILSE).await;
    assert_eq!(
        body["played_with"],
        json!([
            // #2961 : `shared_albums` et `album_wide` (Nora, artiste des 4
            // pistes de son album de 4 : album entier).
            { "artist_id": 2, "artist_name": "Nora Vidal", "shared_tracks": 4, "shared_albums": 1, "album_wide": true },
            { "artist_id": null, "artist_name": "Tomas Reyes", "shared_tracks": 2, "shared_albums": 1, "album_wide": false },
            { "artist_id": 9, "artist_name": "ana bell", "shared_tracks": 1, "shared_albums": 1, "album_wide": false },
            { "artist_id": 6, "artist_name": "DJ Okapi", "shared_tracks": 1, "shared_albums": 1, "album_wide": false }
        ]),
        "played_with : {}",
        body["played_with"]
    );
}

#[tokio::test]
async fn various_artists_n_y_est_jamais_7768() {
    let body = joue_avec(bibliotheque(), ILSE).await;
    let n = noms(&body);
    assert!(
        !n.iter().any(|n| n.eq_ignore_ascii_case("Various Artists")),
        "« Various Artists » dans played_with : {n:?}"
    );
}

#[tokio::test]
async fn ni_compositeur_ni_producteur_partage_7768() {
    let body = joue_avec(bibliotheque(), ILSE).await;
    let n = noms(&body);
    assert!(
        !n.contains(&"Lena Park".to_string()),
        "compositrice partagée : {n:?}"
    );
    assert!(
        !n.contains(&"Ruth Adler".to_string()),
        "productrice partagée : {n:?}"
    );
}

#[tokio::test]
async fn jamais_lui_meme_ni_un_album_masque_7768() {
    let body = joue_avec(bibliotheque(), ILSE).await;
    let n = noms(&body);
    for soi in ["Ilse Brandt", "ilse  BRANDT", "I. Brandt"] {
        assert!(!n.contains(&soi.to_string()), "lui-même ({soi}) : {n:?}");
    }
    assert!(
        !n.contains(&"Hidden Friend".to_string()),
        "album masqué : {n:?}"
    );
}

#[tokio::test]
async fn absente_quand_personne_et_tableau_nu_inchange_7768() {
    let body = joue_avec(bibliotheque(), 8).await;
    assert!(body.get("played_with").is_none(), "{body}");
    assert!(body.get("credit_counts").is_some(), "{body}");

    let app = tune_server::routes::router(bibliotheque());
    let (status, nu) = get(&app, "/api/v1/library/artists/1/albums").await;
    assert_eq!(status, StatusCode::OK);
    assert!(nu.is_array(), "sans sections : tableau nu, {nu}");
}

/// Le plafond de 20 : 25 musiciens sur 25 pistes distinctes, de 25 à 1
/// pistes partagées — les 20 premiers, dans l'ordre.
#[tokio::test]
async fn vingt_au_plus_7768() {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    artiste(&s, ILSE, "Ilse Brandt");
    artiste(&s, 2, "Nora Vidal");
    album(&s, 100, 2, false);
    pistes(&s, 100, 9, 2);
    album(&s, 101, 2, false);
    pistes(&s, 101, 9, 2);
    album(&s, 102, 2, false);
    pistes(&s, 102, 7, 2);
    let toutes: Vec<i64> = (1001..=1009)
        .chain(1011..=1019)
        .chain(1021..=1027)
        .collect();
    for p in &toutes {
        credit(
            &s,
            *p,
            Some(ILSE),
            "Ilse Brandt",
            None,
            "performer",
            "musicbrainz",
        );
    }
    for k in 1..=24usize {
        for p in &toutes[..k] {
            credit(
                &s,
                *p,
                None,
                &format!("Musicien {k:02}"),
                None,
                "performer",
                "musicbrainz",
            );
        }
    }
    let body = joue_avec(s, ILSE).await;
    let liste = body["played_with"].as_array().unwrap();
    assert_eq!(liste.len(), 20);
    assert_eq!(liste[0]["artist_name"], "Nora Vidal");
    assert_eq!(liste[0]["shared_tracks"], 25);
    assert_eq!(liste[1]["artist_name"], "Musicien 24");
    assert_eq!(liste[19]["artist_name"], "Musicien 06");
}

/// #2961 (web) — le cas réel du .18 : un duo « A & B », un album servi deux
/// fois (local et copie UPnP, deux lignes `albums` de même titre), des
/// balises qui recopient le personnel de l'album sur presque chaque piste.
/// Avant : « 20 pistes en commun » pour tous (13 + 7 copies) et les deux
/// membres du duo dans sa propre liste.
fn bibliotheque_duo() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    artiste(&s, 50, "DjeuhDjoah & Lieutenant Nicholson");
    artiste(&s, 51, "Lieutenant Nicholson");
    for (album, source) in [(700, "local"), (701, "upnp")] {
        exec(
            &s,
            &format!(
                "INSERT INTO albums (id, title, artist_id, year, source, release_type, is_compilation) \
                 VALUES ({album}, 'Aimez ces airs', 50, 2021, '{source}', 'album', 0)"
            ),
        );
        for i in 1..=5 {
            let id = album * 10 + i;
            exec(
                &s,
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                     VALUES ({id}, 'Titre {i}', {album}, 50, '/m/{id}.flac', 200000, 'flac', 1, {i})"
                ),
            );
            if i <= 4 {
                for nom in ["Alexis Valet", "Djeuhdjoah", "Lieutenant Nicholson"] {
                    credit(&s, id, None, nom, None, "performer", "balises");
                }
            } else {
                credit(&s, id, None, "Ben Invité", None, "performer", "balises");
            }
        }
    }
    s
}

#[tokio::test]
async fn ni_membres_du_duo_ni_copies_comptees_deux_fois_2961() {
    let body = joue_avec(bibliotheque_duo(), 50).await;
    assert_eq!(
        body["played_with"],
        json!([
            { "artist_id": null, "artist_name": "Alexis Valet", "shared_tracks": 4, "shared_albums": 1, "album_wide": true },
            { "artist_id": null, "artist_name": "Ben Invité", "shared_tracks": 1, "shared_albums": 1, "album_wide": false }
        ]),
        "played_with : {}",
        body["played_with"]
    );

    // Page du membre : le duo dont il est une partie n'y est pas.
    let body = joue_avec(bibliotheque_duo(), 51).await;
    assert_eq!(
        noms(&body),
        vec!["Alexis Valet".to_string(), "Djeuhdjoah".to_string()],
        "{}",
        body["played_with"]
    );
}
