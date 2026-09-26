//! #5160 — les crédits des BALISES du fichier dans le tiroir « Crédits »
//! (Reivax66, fil forum 1965, *Christian McBride's New Jawn*).
//!
//! `GET /library/albums/{id}/credits` et `GET /library/tracks/{id}/credits`
//! ne lisaient que `track_credits`, que seules les passes MusicBrainz et le
//! pont Roon remplissent. Les balises `PERFORMER` / `PRODUCER` du fichier sont
//! rangées au scan dans `track_metadata` (#5043/#5048) : le tiroir ne les
//! montrait jamais.
//!
//! Épreuves contre le VRAI routeur, sur SQLite (en mémoire) et sur une VRAIE
//! base PostgreSQL (`TUNE_TEST_PG_URL` ; absente ⇒ saut annoncé, posée mais
//! injoignable ⇒ le test TOMBE) :
//!
//! - une piste SANS aucune ligne `track_credits` rend ses interprètes et
//!   producteurs de balises, instrument détaché ;
//! - une personne que MusicBrainz crédite déjà au même rôle n'est pas
//!   répétée ; au même nom et à un AUTRE rôle, elle l'est ;
//! - le nom se lie à la fiche d'artiste existante, sinon `artist_id` est nul ;
//! - une clé de `track_metadata` qui n'est pas un crédit (`rg_*`) n'entre pas ;
//! - chaque ligne d'album porte sa piste (`track_title`, numéros).
//!
//! Cible `[[test]]` propre (`autotests = false`).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_server::state::AppState;

async fn get(state: &AppState, path: &str) -> Value {
    let resp = tune_server::routes::router(state.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&body).to_string();
    assert_eq!(status, StatusCode::OK, "{path} : {texte}");
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{path} : JSON illisible ({e}) : {texte}"))
}

/// Ce que l'album pose en base.
struct Bibliotheque {
    album: i64,
    piste_1: i64,
    piste_2: i64,
    mcbride: i64,
}

fn nom(marque: &str, n: &str) -> String {
    format!("{n}{marque}")
}

/// La capture de Reivax66, réduite : piste 1 porte un crédit MusicBrainz
/// (McBride, producteur) ET ses balises ; piste 2 n'a QUE ses balises.
fn poser(state: &AppState, marque: &str) -> Bibliotheque {
    let b = state.backend.clone();
    let id = |sql: &str, p: &[&dyn ToSqlValue]| {
        b.execute_returning_id(sql, p)
            .unwrap_or_else(|e| panic!("{sql} : {e}"))
    };
    let mcbride_nom = nom(marque, "Christian McBride");
    let mcbride = id(
        "INSERT INTO artists (name) VALUES (?)",
        &[&mcbride_nom as &dyn ToSqlValue],
    );
    let titre_album = nom(marque, "New Jawn");
    let local = "local";
    let album = id(
        "INSERT INTO albums (title, artist_id, source) VALUES (?, ?, ?)",
        &[
            &titre_album as &dyn ToSqlValue,
            &mcbride as &dyn ToSqlValue,
            &local as &dyn ToSqlValue,
        ],
    );
    let piste = |titre: &str, numero: i64| {
        let titre = nom(marque, titre);
        let disque = 1i64;
        id(
            "INSERT INTO tracks (title, album_id, artist_id, disc_number, track_number) VALUES (?, ?, ?, ?, ?)",
            &[
                &titre as &dyn ToSqlValue,
                &album as &dyn ToSqlValue,
                &mcbride as &dyn ToSqlValue,
                &disque as &dyn ToSqlValue,
                &numero as &dyn ToSqlValue,
            ],
        )
    };
    let piste_1 = piste("Walkin' Funny", 1);
    let piste_2 = piste("Ke-Kelli Sketch", 2);

    // La ligne MusicBrainz de la piste 1.
    let role = "producer";
    let position = 0i64;
    b.execute(
        "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, NULL, ?)",
        &[
            &piste_1 as &dyn ToSqlValue,
            &mcbride as &dyn ToSqlValue,
            &mcbride_nom as &dyn ToSqlValue,
            &role as &dyn ToSqlValue,
            &position as &dyn ToSqlValue,
        ],
    )
    .expect("crédit MusicBrainz");

    // Les balises, telles que le scan les range (#5048).
    let meta = TrackMetadataRepo::with_backend(b.clone());
    let waits = nom(marque, "Nasheet Waits");
    let whitelock = nom(marque, "Todd Whitelock");
    meta.set(
        piste_1,
        "performer",
        &format!("{mcbride_nom} (bass); {waits} (drums)"),
    )
    .unwrap();
    meta.set(piste_1, "producer", &format!("{mcbride_nom}; {whitelock}"))
        .unwrap();
    meta.set(piste_1, "rg_track_gain", "-3.20 dB").unwrap();
    meta.set(piste_2, "performer", &format!("{mcbride_nom} (bass)"))
        .unwrap();
    Bibliotheque {
        album,
        piste_1,
        piste_2,
        mcbride,
    }
}

type Vu = (i64, String, String, Option<String>, Option<i64>);

fn lire(body: &Value) -> Vec<Vu> {
    body.as_array()
        .unwrap_or_else(|| panic!("pas un tableau : {body}"))
        .iter()
        .map(|l| {
            (
                l["track_id"].as_i64().unwrap(),
                l["role"].as_str().unwrap().to_string(),
                l["artist_name"].as_str().unwrap().to_string(),
                l["instrument"].as_str().map(str::to_string),
                l["artist_id"].as_i64(),
            )
        })
        .collect()
}

async fn epreuve(state: &AppState, marque: &str) {
    let lib = poser(state, marque);
    let n = |x: &str| nom(marque, x);
    let body = get(
        state,
        &format!("/api/v1/library/albums/{}/credits", lib.album),
    )
    .await;
    assert_eq!(
        lire(&body),
        vec![
            // Piste 1 : la ligne MusicBrainz, puis les balises — McBride
            // producteur n'est PAS répété, McBride interprète l'est.
            (
                lib.piste_1,
                "producer".into(),
                n("Christian McBride"),
                None,
                Some(lib.mcbride)
            ),
            (
                lib.piste_1,
                "performer".into(),
                n("Christian McBride"),
                Some("bass".into()),
                Some(lib.mcbride)
            ),
            (
                lib.piste_1,
                "performer".into(),
                n("Nasheet Waits"),
                Some("drums".into()),
                None
            ),
            (
                lib.piste_1,
                "producer".into(),
                n("Todd Whitelock"),
                None,
                None
            ),
            // Piste 2 : aucune ligne `track_credits`, ses balises seules.
            (
                lib.piste_2,
                "performer".into(),
                n("Christian McBride"),
                Some("bass".into()),
                Some(lib.mcbride)
            ),
        ],
        "#5160 — le tiroir de l'album doit rendre les PERFORMER / PRODUCER des balises : {body}"
    );
    let lignes = body.as_array().unwrap();
    let whitelock = lignes
        .iter()
        .find(|l| l["artist_name"] == n("Todd Whitelock").as_str())
        .unwrap();
    assert_eq!(
        whitelock["track_title"],
        n("Walkin' Funny").as_str(),
        "{body}"
    );
    assert_eq!(whitelock["track_number"], 1, "{body}");
    assert_eq!(whitelock["disc_number"], 1, "{body}");
    assert!(
        whitelock["id"].is_null(),
        "ligne hors base : id nul : {body}"
    );
    assert_eq!(whitelock["position"], 3, "position à la suite : {body}");

    let piste = get(
        state,
        &format!("/api/v1/library/tracks/{}/credits", lib.piste_2),
    )
    .await;
    assert_eq!(
        lire(&piste),
        vec![(
            lib.piste_2,
            "performer".into(),
            n("Christian McBride"),
            Some("bass".into()),
            Some(lib.mcbride)
        )],
        "#5160 — le tiroir d'une piste doit rendre ses balises : {piste}"
    );
}

#[tokio::test]
async fn sqlite_les_balises_performer_et_producer_entrent_au_tiroir_5160() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    epreuve(&state, "").await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_les_balises_performer_et_producer_entrent_au_tiroir_5160() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    const MARQUE: &str = " [credits-5160]";
    let menage = |state: &AppState| {
        let motif = format!("%{MARQUE}");
        for sql in [
            "DELETE FROM track_metadata WHERE track_id IN (SELECT id FROM tracks WHERE title LIKE ?)",
            "DELETE FROM track_credits WHERE track_id IN (SELECT id FROM tracks WHERE title LIKE ?)",
            "DELETE FROM tracks WHERE title LIKE ?",
            "DELETE FROM albums WHERE title LIKE ?",
            "DELETE FROM artists WHERE name LIKE ?",
        ] {
            state
                .backend
                .execute(sql, &[&motif as &dyn ToSqlValue])
                .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
        }
    };
    menage(&state);
    epreuve(&state, MARQUE).await;
    menage(&state);
}
