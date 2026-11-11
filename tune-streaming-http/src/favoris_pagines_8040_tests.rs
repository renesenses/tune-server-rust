//! #8040 — les favoris d'un service s'affichent par pages, la première tout
//! de suite. `GET /{service}/favorites/{type}?limit=&offset=` rend UNE page et
//! le total ; sans ces paramètres, la réponse reste la liste entière d'avant,
//! pour les applications qui la demandent ainsi.
//!
//! Deux connecteurs factices : l'un sait lire une page chez le service (comme
//! Tidal), l'autre non — la route découpe alors la liste entière, qu'elle a
//! mise en cache.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, PageFavoris, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack,
    StreamUrl,
};

/// `total` titres favoris, identifiés de `1` à `total`.
struct ServiceAvecFavoris {
    nom: String,
    total: usize,
    /// Sait-il servir une page seule (`get_user_favorites_page`) ?
    sait_paginer: bool,
    /// Lectures de la liste ENTIÈRE (`get_user_tracks`).
    lectures_entieres: AtomicUsize,
    /// Les `(limit, offset)` des pages lues chez le service.
    pages_lues: std::sync::Mutex<Vec<(usize, usize)>>,
}

fn piste(id: usize) -> StreamTrack {
    StreamTrack {
        id: id.to_string(),
        title: format!("piste {id:03}"),
        artist: "Artiste".into(),
        album: None,
        album_id: None,
        duration_ms: 0,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for ServiceAvecFavoris {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        &self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
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
        Ok(Vec::new())
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        self.lectures_entieres.fetch_add(1, Ordering::SeqCst);
        Ok((1..=self.total).map(piste).collect())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_favorites_page(
        &self,
        fav_type: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Option<PageFavoris>, TuneError> {
        if !self.sait_paginer || fav_type != "tracks" {
            return Ok(None);
        }
        self.pages_lues
            .lock()
            .expect("verrou d'essai")
            .push((limit, offset));
        let items = (offset + 1..=self.total)
            .take(limit)
            .map(|id| json!({ "source_id": id.to_string(), "title": format!("piste {id:03}") }))
            .collect();
        Ok(Some(PageFavoris {
            items,
            total: Some(self.total),
        }))
    }
}

/// Chaque essai a son nom de service : les caches de ce module sont des
/// `static` partagés par tout le processus de test.
fn app(nom: &str, total: usize, sait_paginer: bool) -> (Router, Arc<Mutex<ServiceRegistry>>) {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceAvecFavoris {
        nom: nom.to_string(),
        total,
        sait_paginer,
        lectures_entieres: AtomicUsize::new(0),
        pages_lues: std::sync::Mutex::new(Vec::new()),
    }));
    let registre = Arc::new(Mutex::new(registre));
    let etat = StreamingHttpState::new(backend, registre.clone(), Arc::new(EventBus::new()));
    (
        Router::new().nest("/api/v1/streaming", router().with_state(etat)),
        registre,
    )
}

/// Le corps JSON de `GET favorites/tracks{requete}`.
async fn favoris(app: &Router, nom: &str, requete: &str) -> Value {
    use tower::ServiceExt;
    let reponse = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/v1/streaming/{nom}/favorites/tracks{requete}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&corps).unwrap()
}

fn identifiants(corps: &Value) -> Vec<String> {
    corps["tracks"]
        .as_array()
        .expect("tableau tracks")
        .iter()
        .map(|p| {
            p["source_id"]
                .as_str()
                .or_else(|| p["id"].as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

fn ids(de: usize, a: usize) -> Vec<String> {
    (de..=a).map(|i| i.to_string()).collect()
}

/// Ce que le service a vu : (lectures entières, pages lues).
async fn vu(registre: &Arc<Mutex<ServiceRegistry>>, nom: &str) -> (usize, Vec<(usize, usize)>) {
    let arc = registre.lock().await.get(nom).expect("service enregistré");
    let svc = arc.read().await;
    let svc = svc
        .as_any()
        .downcast_ref::<ServiceAvecFavoris>()
        .expect("le connecteur d'essai");
    (
        svc.lectures_entieres.load(Ordering::SeqCst),
        svc.pages_lues.lock().expect("verrou d'essai").clone(),
    )
}

/// Sans `limit` ni `offset`, la réponse est la liste entière, sans clé de
/// pagination : les applications actuelles ne voient aucune différence.
#[tokio::test]
async fn sans_parametres_la_liste_entiere_reste_inchangee_8040() {
    let nom = "entiere-8040";
    let (app, registre) = app(nom, 7, true);
    let corps = favoris(&app, nom, "").await;
    assert_eq!(identifiants(&corps), ids(1, 7));
    assert!(
        corps.get("total").is_none() && corps.get("offset").is_none(),
        "#8040 : sans paramètres, la réponse d'avant, octet pour octet : {corps}"
    );
    let (entieres, pages) = vu(&registre, nom).await;
    assert_eq!(
        (entieres, pages),
        (1, Vec::new()),
        "la liste entière, lue une fois, chez le service"
    );
}

/// Un connecteur qui sait servir une page : la première page part chez lui
/// en UNE requête, avec le total, sans lire les autres pages.
#[tokio::test]
async fn la_premiere_page_part_chez_le_connecteur_qui_sait_paginer_8040() {
    let nom = "pagine-8040";
    let (app, registre) = app(nom, 120, true);
    let corps = favoris(&app, nom, "?limit=50&offset=0").await;
    assert_eq!(identifiants(&corps), ids(1, 50), "{corps}");
    assert_eq!(
        corps["total"],
        json!(120),
        "#8040 : le total annoncé par le service"
    );
    assert_eq!(corps["limit"], json!(50));
    assert_eq!(corps["offset"], json!(0));
    let (entieres, pages) = vu(&registre, nom).await;
    assert_eq!(
        (entieres, pages),
        (0, vec![(50, 0)]),
        "#8040 : une page demandée = une page lue, pas la liste entière"
    );
}

/// Une page lue chez le service ne remplit pas le cache : la demande de la
/// liste entière qui suit repart au service, elle ne ressort pas 50 favoris
/// sur 120.
#[tokio::test]
async fn une_page_ne_remplit_pas_le_cache_de_la_liste_entiere_8040() {
    let nom = "page-puis-entiere-8040";
    let (app, registre) = app(nom, 120, true);
    let page = favoris(&app, nom, "?limit=50&offset=0").await;
    assert_eq!(identifiants(&page).len(), 50);
    let entiere = favoris(&app, nom, "").await;
    assert_eq!(
        identifiants(&entiere).len(),
        120,
        "#8040 : la liste entière après une page doit être entière : {entiere}"
    );
    assert!(entiere.get("total").is_none());
    let (entieres, _) = vu(&registre, nom).await;
    assert_eq!(
        entieres, 1,
        "la liste entière est lue chez le service, pas prise d'une page"
    );
}

/// Un connecteur qui ne sait pas paginer : la route lit la liste entière,
/// la met en cache et la découpe ; la page suivante sort du cache.
#[tokio::test]
async fn un_connecteur_sans_page_voit_sa_liste_decoupee_par_la_route_8040() {
    let nom = "decoupe-8040";
    let (app, registre) = app(nom, 7, false);
    let corps = favoris(&app, nom, "?limit=3&offset=2").await;
    assert_eq!(identifiants(&corps), ids(3, 5), "{corps}");
    assert_eq!(corps["total"], json!(7));
    let suite = favoris(&app, nom, "?limit=3&offset=5").await;
    assert_eq!(identifiants(&suite), ids(6, 7), "{suite}");
    assert_eq!(suite["total"], json!(7));
    let (entieres, pages) = vu(&registre, nom).await;
    assert_eq!(
        (entieres, pages),
        (1, Vec::new()),
        "#8040 : une seule lecture entière, les pages suivantes sortent du cache"
    );
}

/// Une page AVEC tri (#2001) ne peut pas se lire chez le service : le tri
/// porte sur la liste entière, qui est lue puis triée puis découpée.
#[tokio::test]
async fn une_page_triee_se_decoupe_apres_le_tri_de_la_liste_entiere_8040() {
    let nom = "page-triee-8040";
    let (app, registre) = app(nom, 7, true);
    let corps = favoris(&app, nom, "?limit=2&offset=0&sort=title&order=desc").await;
    assert_eq!(
        identifiants(&corps),
        vec!["7".to_string(), "6".to_string()],
        "#8040 : les deux derniers titres par ordre alphabétique inverse : {corps}"
    );
    assert_eq!(corps["total"], json!(7));
    let (entieres, pages) = vu(&registre, nom).await;
    assert_eq!(
        (entieres, pages),
        (1, Vec::new()),
        "#8040 : avec un tri, la liste entière est lue, jamais une page seule"
    );
}

/// `limit` absent avec `offset` : une page de la taille par défaut ;
/// `limit=0` est ramené à 1 — jamais une liste vide ni une erreur.
#[tokio::test]
async fn une_limite_absente_ou_nulle_vaut_la_page_par_defaut_8040() {
    let nom = "limite-defaut-8040";
    let (app, _registre) = app(nom, 120, false);
    let corps = favoris(&app, nom, "?offset=100").await;
    assert_eq!(identifiants(&corps), ids(101, 120), "{corps}");
    assert_eq!(corps["limit"], json!(PageDemandee::LIMITE_DEFAUT));
    let corps = favoris(&app, nom, "?limit=0").await;
    assert_eq!(
        identifiants(&corps).len(),
        1,
        "#8040 : limit=0 est ramené à 1 : {corps}"
    );
}
