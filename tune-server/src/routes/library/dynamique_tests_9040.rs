//! #9040 — la dynamique d'un album (`GET /library/albums/{id}/dynamique`) et
//! la section `dynamique` de la fiche technique, par le vrai routeur.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::audio::plage_de_sonie::{self, Histogramme, PlageDeSonie};

use crate::state::AppState;

fn executer(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Une piste de 100 fenêtres de 3 s, toutes à `lufs` : LRA 0.
fn constante(lufs: f64) -> PlageDeSonie {
    PlageDeSonie {
        lra_lu: 0.0,
        histogramme: Histogramme::de(&vec![lufs; 100]),
    }
}

fn ecrire(state: &AppState, track_id: i64, p: &PlageDeSonie) {
    struct Direct<'a>(&'a AppState);
    impl tune_core::db::backend::DbTxHandle for Direct<'_> {
        fn execute(
            &self,
            sql: &str,
            params: &[&dyn tune_core::db::backend::ToSqlValue],
        ) -> Result<usize, String> {
            self.0.backend.execute(sql, params)
        }
        fn query_one(
            &self,
            sql: &str,
            params: &[&dyn tune_core::db::backend::ToSqlValue],
        ) -> Result<Option<Vec<tune_core::db::backend::SqlValue>>, String> {
            self.0.backend.query_one(sql, params)
        }
        fn query_many(
            &self,
            sql: &str,
            params: &[&dyn tune_core::db::backend::ToSqlValue],
        ) -> Result<Vec<Vec<tune_core::db::backend::SqlValue>>, String> {
            self.0.backend.query_many(sql, params)
        }
        fn last_insert_rowid(&self) -> i64 {
            self.0.backend.last_insert_rowid()
        }
    }
    plage_de_sonie::ecrire(&Direct(state), track_id, p).unwrap();
}

/// Album 1 : pistes 1 et 2 à fichier, une piste de service 3 (hors compte).
fn semer(state: &AppState) {
    executer(state, "INSERT INTO albums (id, title) VALUES (1, 'Album')");
    executer(
        state,
        "INSERT INTO tracks (id, title, album_id, file_path, duration_ms, source) VALUES \
         (1, 'un', 1, '/m/1.flac', 60000, 'local'), \
         (2, 'deux', 1, '/m/2.flac', 60000, 'local'), \
         (3, 'trois', 1, NULL, 60000, 'qobuz')",
    );
    executer(
        state,
        "INSERT INTO track_metadata (track_id, key, value) VALUES \
         (1, 'dr_track', '9'), (1, 'dr_source', 'analysis'), \
         (2, 'dr_track', '12'), (2, 'dr_source', 'analysis'), \
         (1, 'rg_track_source', 'analysis'), (1, 'rg_track_gain', '-3.20 dB'), \
         (1, 'rg_track_peak', '0.794328'), (1, 'rg_track_true_peak', '0.891251'), \
         (1, 'rg_algo', 'bs1770-tp4x-v2'), (1, 'rg_analyzed', '1762682400'), \
         (1, 'rg_album_source', 'analysis'), (2, 'rg_album_source', 'analysis'), \
         (1, 'rg_album_gain', '-4.00 dB'), (2, 'rg_album_gain', '-4.00 dB'), \
         (1, 'rg_album_peak', '0.891251'), (2, 'rg_album_peak', '0.891251'), \
         (1, 'rg_album_true_peak', '1.122018'), (2, 'rg_album_true_peak', '1.122018')",
    );
}

#[tokio::test]
async fn album_dr_sonie_cretes_et_lra_sur_toutes_ses_fenetres_9040() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    semer(&state);
    let app = crate::routes::router(state.clone());

    // Une seule piste mesurée : pas de LRA d'album, faite d'une moitié.
    ecrire(&state, 1, &constante(-20.0));
    let (statut, a) = get(&app, "/api/v1/library/albums/1/dynamique").await;
    assert_eq!(statut, StatusCode::OK, "{a}");
    assert_eq!(a["pistes"], 2, "{a}");
    assert!(a["lra_lu"].is_null(), "{a}");
    assert_eq!(a["lra_pistes_mesurees"], 1);

    // Les deux : −20 et −30 LUFS constants, LRA 0 chacune, album 10.
    ecrire(&state, 2, &constante(-30.0));
    let (_, a) = get(&app, "/api/v1/library/albums/1/dynamique").await;
    assert_eq!(a["lra_lu"], 10.0, "{a}");
    assert_eq!(a["lra_pistes_mesurees"], 2);
    assert_eq!(a["lra_algo"], plage_de_sonie::ALGO);
    // DR d'album : la moyenne arrondie des pistes, (9 + 12) / 2 → 11.
    assert_eq!(a["dr"], 11, "{a}");
    assert_eq!(a["dr_du_tag_d_album"], false);
    // −18 − (−4) = −14 LUFS ; 0,891251 = −1 dBFS ; 1,122018 = +1 dBTP.
    assert_eq!(a["integree_lufs"], -14.0, "{a}");
    assert_eq!(a["crete_echantillon_dbfs"], -1.0);
    assert_eq!(a["crete_vraie_dbtp"], 1.0);

    // Un histogramme illisible ne rentre pas dans la LRA de l'album.
    executer(
        &state,
        "UPDATE plages_de_sonie SET histogramme = 'abc' WHERE track_id = 2",
    );
    let (_, a) = get(&app, "/api/v1/library/albums/1/dynamique").await;
    assert!(a["lra_lu"].is_null(), "{a}");

    // Album inconnu : rien de mesuré, aucune valeur inventée.
    let (statut, a) = get(&app, "/api/v1/library/albums/404/dynamique").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(a["pistes"], 0);
    for k in ["dr", "integree_lufs", "crete_vraie_dbtp", "lra_lu"] {
        assert!(a[k].is_null(), "{k} : {a}");
    }
}

#[tokio::test]
async fn fiche_technique_reunit_la_dynamique_de_la_piste_9040() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    semer(&state);
    ecrire(
        &state,
        1,
        &PlageDeSonie {
            lra_lu: 7.3,
            histogramme: Histogramme::de(&[-20.0, -27.3]),
        },
    );
    // La même écriture deux fois remplace, ne double pas.
    ecrire(
        &state,
        1,
        &PlageDeSonie {
            lra_lu: 7.4,
            histogramme: Histogramme::de(&[-20.0, -27.4]),
        },
    );
    let app = crate::routes::router(state);
    let (statut, f) = get(&app, "/api/v1/library/tracks/1/fiche-technique").await;
    assert_eq!(statut, StatusCode::OK, "{f}");
    let d = &f["dynamique"];
    assert_eq!(d["dr"], 9, "{d}");
    assert_eq!(d["dr_source"], "analysis");
    assert_eq!(d["integree_lufs"], -14.8);
    assert_eq!(d["crete_echantillon_dbfs"], -2.0);
    assert_eq!(d["crete_vraie_dbtp"], -1.0);
    assert_eq!(d["sonie_algo"], "bs1770-tp4x-v2");
    assert_eq!(d["lra_lu"], 7.4);
    assert_eq!(d["lra_algo"], plage_de_sonie::ALGO);
    assert!(d["lra_mesuree_le"].as_str().is_some());

    // Piste 2 : un DR, pas de mesure ReplayGain de Tune, pas de LRA.
    let (_, f) = get(&app, "/api/v1/library/tracks/2/fiche-technique").await;
    let d = &f["dynamique"];
    assert_eq!(d["dr"], 12, "{d}");
    for k in ["integree_lufs", "crete_vraie_dbtp", "lra_lu"] {
        assert!(d.get(k).is_none(), "{k} : {d}");
    }
    // Piste de service : aucune section.
    let (_, f) = get(&app, "/api/v1/library/tracks/3/fiche-technique").await;
    assert!(f["dynamique"].is_null(), "{f}");
}
