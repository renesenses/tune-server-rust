//! #7619 (L1) — `GET /library/tracks` par CURSEUR, de bout en bout.
//!
//! * la page triée (`order=asc`) et la vue par défaut (sans paramètre) rendent
//!   `next_cursor` ; le suivre parcourt la vue entière, sans doublon ni trou,
//!   avec le même total, et les mêmes pistes que l'ancien chemin ;
//! * `after` illisible, ou rendu pour un autre tri : 400, jamais ignoré.

use std::collections::HashSet;

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::track_repo::TrackRepo;

use super::facettes_lentes_2144_tests::{Profil, remplir};
use crate::state::AppState;

async fn get(state: &AppState, chemin: &str) -> (u16, Value) {
    let r = super::super::router()
        .with_state(state.clone())
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = r.status().as_u16();
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&corps).unwrap_or(Value::Null),
    )
}

fn base() -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("curseur-7619.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(
        &state,
        &Profil {
            albums: 60,
            pistes: 9,
            copie_tous: 7,
            cles_de_remplissage: false,
        },
    );
    (dossier, state)
}

async fn parcourir(state: &AppState, premiere: &str) -> (Vec<i64>, i64) {
    let mut ids = Vec::new();
    let mut url = premiere.to_string();
    for _ in 0..1_000 {
        let (statut, v) = get(state, &url).await;
        assert_eq!(statut, 200, "{url} : {v}");
        ids.extend(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_i64().unwrap()),
        );
        match v["next_cursor"].as_str() {
            Some(c) => url = format!("{premiere}&after={c}"),
            None => return (ids, v["total"].as_i64().unwrap()),
        }
    }
    panic!("le parcours ne s'arrête pas");
}

#[tokio::test]
async fn le_curseur_parcourt_la_vue_entiere_7619() {
    let (_d, state) = base();
    let avant: HashSet<i64> = TrackRepo::with_backend(state.backend.clone())
        .list_visible_avec_total(1_000_000, 0)
        .unwrap()
        .0
        .iter()
        .filter_map(|t| t.id)
        .collect();
    for premiere in ["/tracks?limit=37&order=asc", "/tracks?limit=37"] {
        let (ids, total) = parcourir(&state, premiere).await;
        let uniques: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(uniques.len(), ids.len(), "{premiere} : doublon");
        assert_eq!(
            uniques, avant,
            "{premiere} : pas les pistes de l'ancien chemin"
        );
        assert_eq!(total as usize, avant.len(), "{premiere} : total");
    }
}

#[tokio::test]
async fn un_curseur_illisible_ou_d_un_autre_tri_rend_400_7619() {
    let (_d, state) = base();
    let (statut, _) = get(&state, "/tracks?limit=5&order=asc&after=%21%21").await;
    assert_eq!(statut, 400);
    let (_, v) = get(&state, "/tracks?limit=5&order=asc").await;
    let c = v["next_cursor"].as_str().unwrap().to_string();
    let (statut, _) = get(&state, &format!("/tracks?limit=5&sort=title&after={c}")).await;
    assert_eq!(
        statut, 400,
        "un curseur de l'ordre par défaut sous un tri par titre"
    );
}
