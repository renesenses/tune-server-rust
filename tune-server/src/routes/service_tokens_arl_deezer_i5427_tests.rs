//! #5427 — l'ARL saisi dans « Accès et jetons » doit atteindre Deezer.
//!
//! Fil 2035 (v0.9.167, Docker) : l'ARL enregistré par
//! `POST /api/v1/services/tokens/deezer` était rangé dans `settings.deezer_arl`
//! et dans `streaming_auth`, que `DeezerService` ne lit jamais ; la route
//! répondait « Pas de validation disponible. » sans avoir rien demandé à
//! Deezer. La carte Streaming, elle, restait « Non connecté ».
//!
//! Ces témoins attaquent la ROUTE MONTÉE par `crate::routes::router`, devant
//! un faux service `deezer` (aucun réseau) qui note ce qu'on lui remet. L'ARL
//! employé est factice.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

/// ARL factice, de la longueur d'un vrai (~192 caractères).
fn arl_factice() -> String {
    "F".repeat(192)
}

/// Le faux Deezer : il accepte `arl_factice()` et refuse tout le reste, comme
/// la passerelle refuse un ARL expiré.
struct FauxDeezer {
    recus: Arc<Mutex<Vec<Value>>>,
    arl: Option<String>,
}

#[async_trait::async_trait]
impl StreamingService for FauxDeezer {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "deezer"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, c: &Value) -> Result<AuthStatus, TuneError> {
        self.recus.lock().unwrap().push(c.clone());
        match c["arl"].as_str() {
            Some(arl) if arl == arl_factice() => {
                self.arl = Some(arl.to_string());
                Ok(self.auth_status().await)
            }
            Some(_) => Err("deezer: ARL refusé par Deezer (ARL invalide ou expiré)".into()),
            None => Err("deezer: aucun ARL enregistré".into()),
        }
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.arl.is_some(),
            username: self.arl.as_ref().map(|_| "testeur".to_string()),
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    fn save_tokens(&self) -> Option<Value> {
        self.arl.as_ref().map(|a| json!({ "arl": a }))
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}

async fn app() -> (axum::Router, crate::state::AppState, Arc<Mutex<Vec<Value>>>) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let recus = Arc::new(Mutex::new(Vec::new()));
    state.services.lock().await.register(Box::new(FauxDeezer {
        recus: recus.clone(),
        arl: None,
    }));
    (crate::routes::router(state.clone()), state, recus)
}

async fn post(app: &axum::Router, chemin: &str, corps: Value) -> (Value, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let brut = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&brut).to_string();
    assert_eq!(statut, StatusCode::OK, "{chemin} : {texte}");
    (serde_json::from_str(&texte).unwrap(), texte)
}

/// ⭐ Le cas du fil 2035 : l'ARL enregistré dans « Accès et jetons » est
/// REMIS au service Deezer, la session ouverte est persistée là où le
/// démarrage la relit, et la réponse le dit.
#[tokio::test]
async fn l_arl_enregistre_dans_acces_et_jetons_authentifie_deezer() {
    let (app, state, recus) = app().await;
    let (rep, texte) = post(
        &app,
        "/api/v1/services/tokens/deezer",
        json!({ "arl": arl_factice() }),
    )
    .await;

    let recus = recus.lock().unwrap().clone();
    assert_eq!(
        recus.len(),
        1,
        "l'ARL enregistré n'a jamais été remis au service Deezer (#5427) ; réponse : {texte}"
    );
    assert_eq!(recus[0]["arl"], arl_factice());
    assert_eq!(rep["valid"], true, "réponse : {texte}");

    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert!(
        settings.get("auth_tokens_deezer").unwrap().is_some(),
        "la session Deezer doit être persistée dans `auth_tokens_deezer`, la ligne que le démarrage restaure"
    );
    assert!(
        settings.get("deezer_arl").unwrap().is_none(),
        "`settings.deezer_arl` n'est lu par personne : l'ARL n'a pas à y être recopié"
    );
    assert!(
        !texte.contains(&arl_factice()),
        "la réponse ne doit jamais renvoyer la valeur de l'ARL"
    );

    // « Tester » dit ce que le service en a fait.
    let (test, _) = post(&app, "/api/v1/services/tokens/deezer/test", json!({})).await;
    assert_eq!(test["valid"], true, "test : {test}");
}

/// Un ARL que Deezer refuse rend `valid: false` et un message qui le dit —
/// plus « Pas de validation disponible. », plus « app_id required ».
#[tokio::test]
async fn un_arl_refuse_par_deezer_rend_une_erreur_claire() {
    let (app, _state, recus) = app().await;
    let mauvais = "M".repeat(192);
    let (rep, texte) = post(
        &app,
        "/api/v1/services/tokens/deezer",
        json!({ "arl": mauvais }),
    )
    .await;

    assert_eq!(recus.lock().unwrap().len(), 1, "réponse : {texte}");
    assert_eq!(rep["valid"], false, "réponse : {texte}");
    let message = rep["validation_message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ARL refusé par Deezer"),
        "le refus doit être nommé : {message}"
    );
    assert!(!message.contains("app_id"), "{message}");
    assert!(
        !texte.contains(&mauvais),
        "la réponse ne doit jamais renvoyer l'ARL"
    );
}
