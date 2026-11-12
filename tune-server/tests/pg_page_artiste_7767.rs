//! « Écriture et production » et `credit_counts` de la page artiste sur une
//! VRAIE base PostgreSQL (#7767).
//!
//! Jumeau du témoin SQLite `tests/page_artiste_sections_7767.rs`, réduit à ce
//! que PostgreSQL peut contredire : les trois branches `UNION` de
//! `credits_hors_discographie` (fiche en `CAST(... AS TEXT)`, nom, MBID),
//! l'`EXISTS` qui rend un BOOLÉEN (un entier sur SQLite), les
//! `COUNT(DISTINCT)` en BIGINT, et le vrai routeur au bout. L'`EXPLAIN` de
//! la requête des compteurs est imprimé (`--nocapture`) : sur une table de
//! banc minuscule, le planificateur préfère un parcours séquentiel, on ne
//! l'assert pas.
//!
//! Même doctrine que `pg_credits_4984.rs` : variable ABSENTE ⇒ saut annoncé ;
//! variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::metadata::credits_release::{
    classer, compteurs_de_credits, credits_hors_discographie,
};
use tune_server::state::AppState;

/// Suffixe propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide rien et on ne compte que nos lignes.
const MARQUE: &str = "page-artiste-7767";

fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(b: &dyn DbBackend) {
    for sql in [
        format!(
            "DELETE FROM track_credits WHERE track_id IN \
             (SELECT id FROM tracks WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM tracks WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

fn artiste(b: &dyn DbBackend, nom: &str, mbid: Option<&str>) -> i64 {
    let nom = format!("{MARQUE} {nom}");
    b.execute_returning_id(
        "INSERT INTO artists (name, musicbrainz_id) VALUES (?, ?)",
        &[&nom as &dyn ToSqlValue, &mbid as &dyn ToSqlValue],
    )
    .unwrap_or_else(|e| panic!("artiste {nom} : {e}"))
}

fn album(b: &dyn DbBackend, titre: &str, artiste: i64, annee: i64) -> i64 {
    let titre = format!("{MARQUE} {titre}");
    b.execute_returning_id(
        "INSERT INTO albums (title, artist_id, year, source, release_type) VALUES (?, ?, ?, 'local', 'album')",
        &[
            &titre as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
            &annee as &dyn ToSqlValue,
        ],
    )
    .unwrap_or_else(|e| panic!("album {titre} : {e}"))
}

fn pistes(b: &dyn DbBackend, album: i64, artiste: i64, n: i64) -> Vec<i64> {
    (1..=n)
        .map(|i| {
            let titre = format!("{MARQUE} piste {album}-{i}");
            b.execute_returning_id(
                "INSERT INTO tracks (title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                 VALUES (?, ?, ?, ?, 200000, 'flac', 1, ?)",
                &[
                    &titre as &dyn ToSqlValue,
                    &album as &dyn ToSqlValue,
                    &artiste as &dyn ToSqlValue,
                    &titre as &dyn ToSqlValue,
                    &i as &dyn ToSqlValue,
                ],
            )
            .unwrap_or_else(|e| panic!("piste {titre} : {e}"))
        })
        .collect()
}

fn credit(
    b: &dyn DbBackend,
    piste: i64,
    artiste: Option<i64>,
    nom: &str,
    mbid: Option<&str>,
    role: &str,
    instrument: Option<&str>,
) {
    let nom = format!("{MARQUE} {nom}");
    b.execute(
        "INSERT INTO track_credits (track_id, artist_id, artist_name, artist_mbid, role, instrument, position, source) \
         VALUES (?, ?, ?, ?, ?, ?, 0, 'musicbrainz')",
        &[
            &piste as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
            &nom as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
            &role as &dyn ToSqlValue,
            &instrument as &dyn ToSqlValue,
        ],
    )
    .unwrap_or_else(|e| panic!("crédit {nom} / {role} : {e}"));
}

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

fn titres(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("pas un tableau : {v}"))
        .iter()
        .map(|a| {
            a["title"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches(MARQUE)
                .trim()
                .to_string()
        })
        .collect()
}

const MBID_ILSE: &str = "0c7a9d4e-7767-4c1a-9b2e-000000000412";

/// La musicienne de jazz du contrat (§ 6.1), reconnue par ses TROIS clés :
/// fiche sur son disque et *Fixture Jazz Session*, MBID seul sur *Second
/// Session*, nom seul sur *Lena Park Sings* — plus *Guest Session*, où elle
/// signe une piste et en produit une autre.
#[tokio::test(flavor = "multi_thread")]
async fn pg_7767_ecriture_et_compteurs_de_la_page_artiste() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let b: &dyn DbBackend = state.backend.as_ref();
    menage(b);

    let ilse = artiste(b, "Ilse Brandt", Some(MBID_ILSE));
    let quartet = artiste(b, "Nora Vidal Quartet", None);
    let lena = artiste(b, "Lena Park", None);

    // Discographie : 8 pistes, elle en signe 4 (fiche).
    let low_tide = album(b, "Low Tide", ilse, 2019);
    let p = pistes(b, low_tide, ilse, 8);
    for t in &p[..4] {
        credit(b, *t, Some(ilse), "Ilse Brandt", None, "composer", None);
    }
    // Apparaît sur : contrebasse sur tout Fixture Jazz Session (fiche).
    let session = album(b, "Fixture Jazz Session", quartet, 2018);
    for t in pistes(b, session, quartet, 7) {
        credit(
            b,
            t,
            Some(ilse),
            "Ilse Brandt",
            None,
            "performer",
            Some("bass"),
        );
    }
    // Second Session : contrebasse sur 3 pistes, productrice de tout (MBID
    // seul, sous un autre nom).
    let seconde = album(b, "Second Session", quartet, 2022);
    let p = pistes(b, seconde, quartet, 9);
    for t in &p[..3] {
        credit(
            b,
            *t,
            None,
            "I. Brandt",
            Some(MBID_ILSE),
            "performer",
            Some("bass"),
        );
    }
    for t in &p {
        credit(b, *t, None, "I. Brandt", Some(MBID_ILSE), "producer", None);
    }
    // Lena Park Sings : deux compositions (nom exact seul).
    let sings = album(b, "Lena Park Sings", lena, 2023);
    let p = pistes(b, sings, lena, 11);
    for t in [p[1], p[8]] {
        credit(b, t, None, "Ilse Brandt", None, "composer", None);
    }

    // Guest Session : elle est l'ARTISTE d'une piste (« Apparitions ») et
    // productrice d'une autre — l'`EXISTS` booléen à VRAI, le disque gardé
    // par « Écriture et production » seulement (§ 3.1 du contrat).
    let guest = album(b, "Guest Session", quartet, 2020);
    let p = pistes(b, guest, quartet, 2);
    b.execute(
        "UPDATE tracks SET artist_id = ? WHERE id = ?",
        &[&ilse as &dyn ToSqlValue, &p[0] as &dyn ToSqlValue],
    )
    .expect("artiste de la piste");
    credit(b, p[1], Some(ilse), "Ilse Brandt", None, "producer", None);
    let produite = p[1];

    // La lecture pure : l'`EXISTS` booléen est lu, les disques classés.
    let (credits, disques) = credits_hors_discographie(&state.backend, ilse);
    assert_eq!(credits.len(), 7 + 3 + 9 + 2 + 1, "{credits:?}");
    assert_eq!(
        disques
            .iter()
            .filter(|d| d.artiste_de_piste)
            .map(|d| d.album_id)
            .collect::<Vec<_>>(),
        vec![guest],
        "seule Guest Session porte une piste signée par elle : {disques:?}"
    );
    let c = classer(&credits, &disques);
    assert_eq!(
        c.collaborations
            .iter()
            .map(|a| a.album_id)
            .collect::<Vec<_>>(),
        vec![session, seconde]
    );
    assert_eq!(
        c.reprises.iter().map(|a| a.album_id).collect::<Vec<_>>(),
        vec![sings]
    );
    assert_eq!(
        c.ecriture.iter().map(|a| a.album_id).collect::<Vec<_>>(),
        vec![seconde, sings, guest]
    );
    assert_eq!(c.ecriture[0].track_ids.len(), 9);
    assert_eq!(c.ecriture[0].roles, vec!["producer"]);

    // Les compteurs : interprète 4 albums, 19 pistes (8 + 7 + 3 + 1) ;
    // écriture 4 albums, 16 pistes (4 + 9 + 2 + 1) — en BIGINT, lus en i64.
    let compteurs = compteurs_de_credits(&state.backend, ilse);
    assert_eq!(
        (compteurs.performing.albums, compteurs.performing.tracks),
        (4, 19),
        "{compteurs:?}"
    );
    assert_eq!(
        (compteurs.writing.albums, compteurs.writing.tracks),
        (4, 16),
        "{compteurs:?}"
    );

    // Le vrai routeur rend le contrat.
    let app = tune_server::routes::router(state.clone());
    let (status, body) = get(
        &app,
        &format!("/api/v1/library/artists/{ilse}/albums?sections=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(titres(&body["albums"]), ["Low Tide"], "{body}");
    assert_eq!(titres(&body["appearances"]), ["Guest Session"], "{body}");
    assert_eq!(titres(&body["covers"]), ["Lena Park Sings"], "{body}");
    // Année puis titre : Guest Session (2020), Second Session (2022), Lena
    // Park Sings (2023).
    assert_eq!(
        titres(&body["writing"]),
        ["Guest Session", "Second Session", "Lena Park Sings"],
        "{body}"
    );
    assert_eq!(
        body["writing"][0]["credit_roles"],
        serde_json::json!(["producer"])
    );
    assert_eq!(
        body["writing"][0]["focus_track_ids"],
        serde_json::json!([produite])
    );
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 4, "tracks": 19 },
            "writing": { "albums": 4, "tracks": 16 }
        }),
        "{body}"
    );
    assert!(body.get("compilations").is_none(), "{body}");
    let collaborations = serde_json::to_string(&body["collaborations"]).unwrap();
    assert!(
        !collaborations.contains("Guest Session"),
        "{collaborations}"
    );

    // Un artiste sans rien : `writing` absente, compteurs à zéro, présents.
    let (_, body) = get(
        &app,
        &format!("/api/v1/library/artists/{lena}/albums?sections=1"),
    )
    .await;
    assert!(body.get("writing").is_none(), "{body}");
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 1, "tracks": 11 },
            "writing": { "albums": 0, "tracks": 0 }
        }),
        "{body}"
    );

    // Le plan de la branche « fiche » des compteurs, pour la revue.
    let plan = b
        .query_many(
            "EXPLAIN SELECT COUNT(DISTINCT t.id), COUNT(DISTINCT t.album_id) \
             FROM tracks t JOIN albums a ON a.id = t.album_id \
             WHERE t.id IN (SELECT track_id FROM track_credits WHERE id IN ( \
                SELECT id FROM track_credits WHERE CAST(artist_id AS TEXT) = ? AND role IN ('composer', 'writer', 'arranger', 'producer', 'engineer', 'mixer') \
                UNION SELECT id FROM track_credits WHERE artist_name = ? AND role IN ('composer', 'writer', 'arranger', 'producer', 'engineer', 'mixer') \
                UNION SELECT id FROM track_credits WHERE artist_mbid = ? AND artist_mbid <> '' AND role IN ('composer', 'writer', 'arranger', 'producer', 'engineer', 'mixer')))",
            &[
                &ilse.to_string() as &dyn ToSqlValue,
                &format!("{MARQUE} Ilse Brandt") as &dyn ToSqlValue,
                &MBID_ILSE as &dyn ToSqlValue,
            ],
        )
        .expect("EXPLAIN des compteurs");
    eprintln!("EXPLAIN credit_counts.writing (#7767) :");
    for ligne in &plan {
        eprintln!(
            "  {}",
            ligne
                .first()
                .and_then(|v| v.as_string())
                .unwrap_or_default()
        );
    }

    menage(b);
}
