//! #5997 (fil 2186, FabienM) — les ROUTES des favoris de service passent par
//! le miroir : un cœur posé ou retiré dans Tune part chez le service, la
//! liste est commune à tous les profils, une panne rend 202 avec son motif.
//!
//! Un service SIMULÉ est inscrit au registre ; aucun réseau. La logique fine
//! (rafraîchissement, adoption, contre-épreuves) est gardée dans
//! `tune-core/src/streaming/favorites_mirror_tests.rs` : ici, on garde le
//! BRANCHEMENT — une route qui n'appellerait plus le miroir ferait rougir.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

#[derive(Default)]
struct Compte {
    appels: Vec<String>,
    panne: bool,
}

struct Simule(Arc<Mutex<Compte>>);

#[async_trait]
impl StreamingService for Simule {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "miroir-route"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    fn favoris_miroir(&self) -> bool {
        true
    }
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(Default::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    /// Le compte relu : ce que le service a reçu en `add` et pas en `remove`.
    async fn get_user_favorites_dated(
        &self,
        fav_type: &str,
    ) -> Result<Option<Vec<Value>>, TuneError> {
        let c = self.0.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        let mut ids: Vec<String> = Vec::new();
        for a in &c.appels {
            let p: Vec<&str> = a.split(' ').collect();
            if p[1] != fav_type {
                continue;
            }
            if p[0] == "add" {
                ids.push(p[2].into());
            } else {
                ids.retain(|i| i != p[2]);
            }
        }
        Ok(Some(
            ids.into_iter()
                .map(|id| json!({"source_id": id, "title": format!("titre {id}"), "artist_name": "a"}))
                .collect(),
        ))
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        Err("panne réseau simulée".into())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Err("panne réseau simulée".into())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Err("panne réseau simulée".into())
    }
    async fn add_favorite(&mut self, t: &str, id: &str) -> Result<(), TuneError> {
        let mut c = self.0.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        c.appels.push(format!("add {t} {id}"));
        Ok(())
    }
    async fn remove_favorite(&mut self, t: &str, id: &str) -> Result<(), TuneError> {
        let mut c = self.0.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        c.appels.push(format!("remove {t} {id}"));
        Ok(())
    }
}

async fn appel(
    app: &axum::Router,
    profil: i64,
    methode: &str,
    path: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", profil.to_string());
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

fn ids(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .expect("la liste doit rester un TABLEAU (anciens clients)")
        .iter()
        .filter(|f| f["service"] == "miroir-route")
        .map(|f| f["service_id"].as_str().unwrap_or_default().to_string())
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn les_routes_de_favoris_passent_par_le_miroir_du_service_5997() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let compte = Arc::new(Mutex::new(Compte::default()));
    state
        .services
        .lock()
        .await
        .register(Box::new(Simule(compte.clone())));
    let deuxieme = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone())
        .create("deuxieme-5997", None, None)
        .unwrap();
    let app = tune_server::routes::router(state.clone());

    // Cœur posé par le profil 1 : ajout CHEZ le service.
    let (s, r) = appel(&app, 1, "POST", "/api/v1/profiles/1/favorites/streaming/add",
        Some(json!({"item_type": "album", "service": "miroir-route", "service_id": "kob", "title": "Kind of Blue"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["miroir"]["statut"], "propage", "{r}");
    assert_eq!(
        compte.lock().unwrap().appels,
        vec!["add albums kob"],
        "la route n'a pas écrit chez le service"
    );

    // Commun à tous les profils : le deuxième le voit.
    let (s, l) = appel(
        &app,
        deuxieme,
        "GET",
        &format!("/api/v1/profiles/{deuxieme}/favorites/streaming"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        ids(&l),
        vec!["kob"],
        "le deuxième profil ne voit pas le favori commun"
    );

    // Retiré par le deuxième : retrait chez le service, disparu pour le premier.
    let (s, r) = appel(
        &app,
        deuxieme,
        "POST",
        &format!("/api/v1/profiles/{deuxieme}/favorites/streaming/remove"),
        Some(json!({"item_type": "album", "service": "miroir-route", "service_id": "kob"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        compte.lock().unwrap().appels.last().map(String::as_str),
        Some("remove albums kob")
    );
    let (_, l) = appel(
        &app,
        1,
        "GET",
        "/api/v1/profiles/1/favorites/streaming",
        None,
    )
    .await;
    assert!(ids(&l).is_empty(), "retiré dans Tune, encore listé : {l}");

    // Panne : 202, motif, rien de perdu.
    compte.lock().unwrap().panne = true;
    let (s, r) = appel(
        &app,
        1,
        "POST",
        "/api/v1/profiles/1/favorites/streaming/add",
        Some(json!({"item_type": "track", "service": "miroir-route", "service_id": "71"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert_eq!(r["miroir"]["statut"], "en_attente");
    assert!(
        r["miroir"]["erreur"]
            .as_str()
            .unwrap_or_default()
            .contains("panne"),
        "{r}"
    );
    let (_, l) = appel(
        &app,
        1,
        "GET",
        "/api/v1/profiles/1/favorites/streaming",
        None,
    )
    .await;
    assert_eq!(
        ids(&l),
        vec!["71"],
        "le cœur posé pendant la panne a été perdu"
    );

    // L'état du miroir le dit.
    let (s, e) = appel(
        &app,
        1,
        "GET",
        "/api/v1/profiles/1/favorites/streaming/miroir",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{e}");
    assert_eq!(e["services"]["miroir-route"]["en_attente"], 1, "{e}");
}
