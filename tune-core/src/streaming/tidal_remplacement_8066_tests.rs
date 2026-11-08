//! #8066 — disponibilité des titres TIDAL et remplacement à sa place dans une
//! playlist, contre un faux TIDAL sur la boucle locale (jeton factice).

use super::*;
use axum::extract::{Path as Chemin, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::sync::Mutex as StdMutex;

#[test]
fn allow_streaming_et_stream_ready_disent_la_disponibilite_8066() {
    let t = |v: serde_json::Value| TidalService::map_track(&v).disponible;
    assert_eq!(
        t(json!({"id": 1, "allowStreaming": false, "streamReady": true})),
        Some(false)
    );
    assert_eq!(
        t(json!({"id": 1, "allowStreaming": true, "streamReady": false})),
        Some(false)
    );
    assert_eq!(
        t(json!({"id": 1, "allowStreaming": true, "streamReady": true})),
        Some(true)
    );
    assert_eq!(t(json!({"id": 1})), None, "rien dit : on ne conclut pas");
}

/// `(type, id)` des éléments, l'étiquette courante et le journal.
struct Etat {
    elements: Vec<(String, u64)>,
    version: u32,
    createur: u64,
    journal: Vec<String>,
}

type Partage = Arc<StdMutex<Etat>>;

fn etiquette(s: &Etat) -> String {
    format!("\"v{}\"", s.version)
}

async fn faux_tidal(elements: &[(&str, u64)]) -> (TidalService, Partage) {
    let etat: Partage = Arc::new(StdMutex::new(Etat {
        elements: elements
            .iter()
            .map(|(t, id)| (t.to_string(), *id))
            .collect(),
        version: 1,
        createur: 42,
        journal: Vec::new(),
    }));

    async fn fiche(
        State(e): State<Partage>,
        Chemin(_id): Chemin<String>,
    ) -> axum::response::Response {
        let s = e.lock().unwrap();
        (
            [(axum::http::header::ETAG, etiquette(&s))],
            axum::Json(json!({"uuid": "pl", "title": "Ma liste", "creator": {"id": s.createur}})),
        )
            .into_response()
    }
    async fn lire(
        State(e): State<Partage>,
        Chemin(_id): Chemin<String>,
    ) -> axum::Json<serde_json::Value> {
        let s = e.lock().unwrap();
        let items: Vec<_> = s
            .elements
            .iter()
            .map(|(t, id)| json!({"type": t, "item": {"id": id, "title": format!("t{id}"), "allowStreaming": true, "streamReady": true}}))
            .collect();
        axum::Json(json!({"items": items, "totalNumberOfItems": items.len()}))
    }
    async fn ajouter(
        State(e): State<Partage>,
        Chemin(_id): Chemin<String>,
        entetes: HeaderMap,
        axum::Form(f): axum::Form<HashMap<String, String>>,
    ) -> axum::response::Response {
        let mut s = e.lock().unwrap();
        if entetes.get("if-none-match").and_then(|v| v.to_str().ok())
            != Some(etiquette(&s).as_str())
        {
            return (StatusCode::PRECONDITION_FAILED, "etag").into_response();
        }
        let a: usize = f["toIndex"].parse().unwrap();
        s.journal.push(format!("add {} at {a}", f["trackIds"]));
        s.elements
            .insert(a, ("track".into(), f["trackIds"].parse().unwrap()));
        s.version += 1;
        axum::Json(json!({"addedItemIds": [f["trackIds"]]})).into_response()
    }
    async fn retirer(
        State(e): State<Partage>,
        Chemin((_id, indice)): Chemin<(String, usize)>,
        entetes: HeaderMap,
    ) -> axum::response::Response {
        let mut s = e.lock().unwrap();
        if entetes.get("if-none-match").and_then(|v| v.to_str().ok())
            != Some(etiquette(&s).as_str())
        {
            return (StatusCode::PRECONDITION_FAILED, "etag").into_response();
        }
        s.journal.push(format!("delete {indice}"));
        s.elements.remove(indice);
        s.version += 1;
        StatusCode::OK.into_response()
    }

    let app = axum::Router::new()
        .route("/playlists/{id}", axum::routing::get(fiche))
        .route(
            "/playlists/{id}/items",
            axum::routing::get(lire).post(ajouter),
        )
        .route(
            "/playlists/{id}/items/{indice}",
            axum::routing::delete(retirer),
        )
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let mut svc = TidalService::pour_un_faux_tidal(format!("http://{adresse}"));
    svc.user_id = Some(42);
    (svc, etat)
}

fn ids(e: &Partage) -> Vec<u64> {
    e.lock()
        .unwrap()
        .elements
        .iter()
        .map(|(_, id)| *id)
        .collect()
}

#[tokio::test]
async fn le_titre_est_remplace_a_sa_place_meme_apres_une_video_8066() {
    // Rang 1 de `get_playlist_tracks` = élément 2 (la vidéo compte chez TIDAL).
    let (svc, etat) = faux_tidal(&[("track", 1), ("video", 500), ("track", 2), ("track", 3)]).await;
    svc.replace_playlist_track("pl", 1, "2", "20")
        .await
        .expect("remplacement");
    assert_eq!(ids(&etat), [1, 500, 20, 3], "position conservée");
    assert_eq!(etat.lock().unwrap().journal, ["add 20 at 2", "delete 3"]);
}

#[tokio::test]
async fn une_playlist_qui_a_change_n_est_pas_ecrite_8066() {
    let (svc, etat) = faux_tidal(&[("track", 1), ("track", 2)]).await;
    assert!(
        svc.replace_playlist_track("pl", 0, "2", "20")
            .await
            .is_err()
    );
    assert!(etat.lock().unwrap().journal.is_empty());
}

#[tokio::test]
async fn modifiable_seulement_par_son_createur_et_jamais_un_mix_8066() {
    let (svc, etat) = faux_tidal(&[("track", 1)]).await;
    assert_eq!(svc.playlist_modifiable("pl").await.unwrap(), Some(true));
    etat.lock().unwrap().createur = 7;
    assert_eq!(svc.playlist_modifiable("pl").await.unwrap(), Some(false));
    assert_eq!(
        svc.playlist_modifiable("mix:abc").await.unwrap(),
        Some(false)
    );
    assert!(
        svc.replace_playlist_track("mix:abc", 0, "1", "2")
            .await
            .is_err()
    );
}
