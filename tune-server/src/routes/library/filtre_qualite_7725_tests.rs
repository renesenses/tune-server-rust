//! #7725 (rc11) — le filtre Qualité de la Bibliothèque : « DSD » et
//! « Multicanal » (décision de Bertrand du 04/11).
//!
//! Banc : six albums d'un même artiste, un par cas.
//!
//! | album        | format | canaux des pistes | DSD | Multicanal |
//! |--------------|--------|-------------------|-----|------------|
//! | Stéréo CD    | flac   | 2, 2              |     |            |
//! | Surround     | flac   | 6, 2              |     | oui        |
//! | DSF stéréo   | dsf    | 2                 | oui |            |
//! | DFF 5.1      | dff    | 6                 | oui | oui        |
//! | SACD ISO     | dsd    | 2                 | oui |            |
//! | Mono         | flac   | 1                 |     |            |
//!
//! * `le_filtre_multicanal_rend_exactement_les_albums_multicanaux_7725` :
//!   `GET /library/albums?quality=multichannel` ;
//! * `le_filtre_dsd_rend_les_dsf_dff_et_iso_7725` : `?quality=dsd` ;
//! * `la_fiche_filtree_multicanal_ne_rend_que_les_pistes_multicanales_7725` :
//!   `GET /library/albums/{id}/tracks?quality=multichannel` ;
//! * `max_channels_est_servi_par_la_grille_et_la_fiche_artiste_7725` : ce que
//!   le client lit pour sa puce Multicanal.

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

use crate::state::AppState;

struct Banc {
    app: axum::Router,
    _state: AppState,
    ids: BTreeMap<&'static str, i64>,
}

/// (titre, format, fréquence, profondeur, canaux de chaque piste)
const ALBUMS: [(&str, &str, i64, i64, &[i64]); 6] = [
    ("Stéréo CD", "flac", 44_100, 16, &[2, 2]),
    ("Surround", "flac", 96_000, 24, &[6, 2]),
    ("DSF stéréo", "dsf", 2_822_400, 1, &[2]),
    ("DFF 5.1", "dff", 2_822_400, 1, &[6]),
    ("SACD ISO", "dsd", 2_822_400, 1, &[2]),
    ("Mono", "flac", 44_100, 16, &[1]),
];

fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // `albums.artist_id` et `tracks.artist_id` référencent `artists(id)`.
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Banc 7725')",
            &[],
        )
        .unwrap();
    let mut ids = BTreeMap::new();
    let mut n = 0;
    for (titre, format, freq, bits, canaux) in ALBUMS {
        state
            .backend
            .execute(
                "INSERT INTO albums (title, artist_id, source, track_count, year, format, \
                 sample_rate, bit_depth) VALUES (?, 1, 'local', ?, 2001, ?, ?, ?)",
                &[
                    &titre as &dyn ToSqlValue,
                    &(canaux.len() as i64),
                    &format,
                    &freq,
                    &bits,
                ],
            )
            .unwrap();
        let album_id = state.backend.last_insert_rowid();
        ids.insert(titre, album_id);
        for (i, c) in canaux.iter().enumerate() {
            n += 1;
            let piste = format!("{titre} — {c} canaux");
            let chemin = format!("/banc7725/{n}.{format}");
            let numero = i as i64 + 1;
            state
                .backend
                .execute(
                    "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path, \
                     file_mtime, duration_ms, format, sample_rate, bit_depth, channels) \
                     VALUES (?, ?, 1, ?, ?, 1000.0, 30000, ?, ?, ?, ?)",
                    &[
                        &piste as &dyn ToSqlValue,
                        &album_id,
                        &numero,
                        &chemin,
                        &format,
                        &freq,
                        &bits,
                        c,
                    ],
                )
                .unwrap();
        }
    }
    let app = crate::routes::router(state.clone());
    Banc {
        app,
        _state: state,
        ids,
    }
}

async fn json(app: &axum::Router, requete: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(requete).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        reponse.status().is_success(),
        "{requete} : {}",
        reponse.status()
    );
    let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&corps).unwrap()
}

impl Banc {
    /// Les titres des albums que rend la grille sous ce filtre de qualité.
    async fn albums_sous(&self, qualite: &str) -> BTreeSet<String> {
        let corps = json(
            &self.app,
            &format!("/api/v1/library/albums?limit=100&offset=0&quality={qualite}"),
        )
        .await;
        corps["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|a| a["title"].as_str().expect("title").to_string())
            .collect()
    }
}

fn titres(t: &[&str]) -> BTreeSet<String> {
    t.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn le_filtre_multicanal_rend_exactement_les_albums_multicanaux_7725() {
    let b = banc();
    assert_eq!(
        b.albums_sous("multichannel").await,
        titres(&["Surround", "DFF 5.1"]),
        "Multicanal doit rendre les albums dont une piste a plus de 2 canaux, et eux seuls"
    );
}

#[tokio::test]
async fn le_filtre_dsd_rend_les_dsf_dff_et_iso_7725() {
    let b = banc();
    assert_eq!(
        b.albums_sous("dsd").await,
        titres(&["DSF stéréo", "DFF 5.1", "SACD ISO"]),
        "DSD doit rendre les DSF, les DFF et les images SACD ISO (format `dsd`)"
    );
    // Un DSD n'est plus compté sous Hi-Res, et inversement.
    assert_eq!(b.albums_sous("hires").await, titres(&["Surround"]));
}

#[tokio::test]
async fn la_fiche_filtree_multicanal_ne_rend_que_les_pistes_multicanales_7725() {
    let b = banc();
    let id = b.ids["Surround"];
    let filtree = json(
        &b.app,
        &format!("/api/v1/library/albums/{id}/tracks?quality=multichannel"),
    )
    .await;
    let canaux: Vec<i64> = filtree
        .as_array()
        .expect("tableau de pistes")
        .iter()
        .map(|t| t["channels"].as_i64().expect("channels"))
        .collect();
    assert_eq!(
        canaux,
        vec![6],
        "sous Multicanal, la fiche ne doit garder que la piste 6 canaux : {filtree}"
    );
    // Sans filtre, la fiche reste entière.
    let entiere = json(&b.app, &format!("/api/v1/library/albums/{id}/tracks")).await;
    assert_eq!(entiere.as_array().expect("pistes").len(), 2);
}

#[tokio::test]
async fn max_channels_est_servi_par_la_grille_et_la_fiche_artiste_7725() {
    let b = banc();
    let attendu: BTreeMap<String, i64> = ALBUMS
        .iter()
        .map(|(t, _, _, _, c)| (t.to_string(), *c.iter().max().unwrap()))
        .collect();

    let grille = json(&b.app, "/api/v1/library/albums?limit=100&offset=0").await;
    let lus: BTreeMap<String, i64> = grille["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|a| {
            (
                a["title"].as_str().unwrap().to_string(),
                a["max_channels"]
                    .as_i64()
                    .unwrap_or_else(|| panic!("grille : max_channels absent sur {a}")),
            )
        })
        .collect();
    assert_eq!(lus, attendu, "grille de la Bibliothèque");

    let fiche = json(&b.app, "/api/v1/library/artists/1/albums").await;
    let lus: BTreeMap<String, i64> = fiche
        .as_array()
        .expect("tableau d'albums")
        .iter()
        .map(|a| {
            (
                a["title"].as_str().unwrap().to_string(),
                a["max_channels"]
                    .as_i64()
                    .unwrap_or_else(|| panic!("fiche artiste : max_channels absent sur {a}")),
            )
        })
        .collect();
    assert_eq!(lus, attendu, "fiche artiste");
}
