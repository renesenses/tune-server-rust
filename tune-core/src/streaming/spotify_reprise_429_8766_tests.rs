//! #8766 (fil 2828) — ce que la rc12 laissait ouvert :
//! 1. la reprise des favoris au démarrage abandonnait sur un `429` au lieu
//!    de se reprogrammer après le `Retry-After` ;
//! 2. chaque rubrique partait deux fois vers Spotify par affichage.
//!
//! Un faux Spotify local (axum sur 127.0.0.1) compte chaque appel. Aucun
//! compte Spotify, aucun son. L'unité de temps est raccourcie
//! (`seconde_429`) : un `Retry-After: 30` y dure 300 ms.

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::streaming::favorites_import::{
    REPRISES_APRES_LIMITE, reprendre_les_favoris_du_service, reprogrammer_apres_les_limites,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

const UNITE: std::time::Duration = std::time::Duration::from_millis(10);

async fn servir(app: axum::Router) -> SpotifyService {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    svc.seconde_429 = UNITE;
    svc
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// `{nom: compte}`, la forme que rend la reprise du serveur.
fn compte_de(
    nom: String,
    compte: crate::streaming::favorites_import::RepriseFavoris,
) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert(nom, serde_json::to_value(compte).unwrap());
    serde_json::Value::Object(o)
}

fn trop_de_demandes() -> Response {
    let mut r = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(json!({"error": {"status": 429, "message": "Too many requests"}})),
    )
        .into_response();
    r.headers_mut()
        .insert("retry-after", HeaderValue::from_static("30"));
    r
}

/// Comment `/me/tracks` répond : les `n` premiers appels en `429`
/// (`usize::MAX` : toujours), ou en `500`.
#[derive(Clone, Copy)]
enum Titres {
    Limite(usize),
    Panne,
}

/// Les favoris d'un compte : un titre, un album, un artiste. `/me/tracks`
/// suit `titres` ; `appels` compte ses appels.
async fn faux_favoris(titres: Titres, appels: Arc<AtomicUsize>) -> SpotifyService {
    let page = |items: serde_json::Value| {
        axum::Json(json!({"items": items, "total": 1, "next": null, "limit": 50, "offset": 0}))
    };
    let app = axum::Router::new()
        .route(
            "/v1/me/tracks",
            get(move || {
                let n = appels.fetch_add(1, Ordering::SeqCst);
                async move {
                    match titres {
                        Titres::Limite(k) if n < k => trop_de_demandes(),
                        Titres::Panne => {
                            (StatusCode::INTERNAL_SERVER_ERROR, "panne").into_response()
                        }
                        _ => page(json!([{"added_at": "2026-11-01T10:00:00Z",
                            "track": {"id": "t1", "name": "titre", "duration_ms": 1000,
                                      "artists": [], "album": {}}}]))
                        .into_response(),
                    }
                }
            }),
        )
        .route(
            "/v1/me/albums",
            get(move || async move {
                page(json!([{"added_at": "2026-11-01T10:00:00Z",
                    "album": {"id": "a1", "name": "album", "artists": [], "images": []}}]))
            }),
        )
        .route(
            "/v1/me/following",
            get(|| async {
                axum::Json(
                    json!({"artists": {"items": [{"id": "r1", "name": "artiste", "images": []}],
                    "next": null, "total": 1, "limit": 50, "cursors": {"after": null}}}),
                )
            }),
        );
    servir(app).await
}

/// Épreuve : `429` + `Retry-After` sur la première lecture. La reprise rend
/// le délai au lieu d'abandonner, puis se refait une fois après lui et lit
/// les trois types.
#[tokio::test]
async fn la_reprise_se_reprogramme_apres_le_retry_after_8766() {
    let appels = Arc::new(AtomicUsize::new(0));
    let svc = faux_favoris(Titres::Limite(1), appels.clone()).await;
    let db = base();

    let premier = reprendre_les_favoris_du_service(&svc, 1, &db).await;
    assert_eq!(premier.lus, 0, "{premier:?}");
    assert_eq!(
        premier.echecs, 3,
        "la pénalité retient aussi albums et artistes : {premier:?}"
    );
    let delai = premier
        .reessayer_dans_s
        .expect("#8766 : le délai du 429 doit être rendu");
    assert!((25..=30).contains(&delai), "Retry-After 30 : {delai}");

    let mut comptes = compte_de("spotify".into(), premier);
    let relus = Arc::new(AtomicUsize::new(0));
    let debut = std::time::Instant::now();
    let reprises = reprogrammer_apres_les_limites(&mut comptes, UNITE, |nom| {
        relus.fetch_add(1, Ordering::SeqCst);
        let (svc, db) = (&svc, &db);
        async move { compte_de(nom, reprendre_les_favoris_du_service(svc, 1, db).await) }
    })
    .await;
    assert!(
        debut.elapsed() >= UNITE * 25,
        "#8766 : la reprise ne doit pas repartir avant la fin du Retry-After"
    );
    assert_eq!(reprises, 1);
    assert_eq!(relus.load(Ordering::SeqCst), 1);
    assert_eq!(comptes["spotify"]["echecs"], 0, "{comptes}");
    assert_eq!(comptes["spotify"]["lus"], 3, "{comptes}");
    assert!(
        comptes["spotify"].get("reessayer_dans_s").is_none(),
        "{comptes}"
    );
    assert_eq!(appels.load(Ordering::SeqCst), 2, "un 429, puis une lecture");
}

/// Contre-épreuve : une panne (`500`) n'est pas une limite. Rien n'est
/// reprogrammé : la reprise de démarrage reste une passe.
#[tokio::test]
async fn une_panne_n_est_pas_reprogrammee_8766() {
    let svc = faux_favoris(Titres::Panne, Arc::new(AtomicUsize::new(0))).await;
    let premier = reprendre_les_favoris_du_service(&svc, 1, &base()).await;
    assert_eq!(premier.echecs, 1, "{premier:?}");
    assert_eq!(premier.lus, 2, "{premier:?}");
    assert_eq!(premier.reessayer_dans_s, None);
    let mut comptes = compte_de("spotify".into(), premier);
    let relus = AtomicUsize::new(0);
    let reprises = reprogrammer_apres_les_limites(&mut comptes, UNITE, |_| {
        relus.fetch_add(1, Ordering::SeqCst);
        async { serde_json::Value::Null }
    })
    .await;
    assert_eq!(reprises, 0);
    assert_eq!(
        relus.load(Ordering::SeqCst),
        0,
        "rien à relire après une panne"
    );
}

/// Contre-épreuve : un Spotify qui limite toujours. La reprise s'arrête après
/// [`REPRISES_APRES_LIMITE`] reprises : pas de veille sans fin.
#[tokio::test]
async fn les_reprises_sont_bornees_8766() {
    let appels = Arc::new(AtomicUsize::new(0));
    let svc = faux_favoris(Titres::Limite(usize::MAX), appels.clone()).await;
    let db = base();
    let premier = reprendre_les_favoris_du_service(&svc, 1, &db).await;
    let mut comptes = compte_de("spotify".into(), premier);
    let reprises = reprogrammer_apres_les_limites(&mut comptes, UNITE, |nom| {
        let (svc, db) = (&svc, &db);
        async move { compte_de(nom, reprendre_les_favoris_du_service(svc, 1, db).await) }
    })
    .await;
    assert_eq!(reprises, REPRISES_APRES_LIMITE);
    assert_eq!(
        appels.load(Ordering::SeqCst),
        1 + REPRISES_APRES_LIMITE as usize,
        "un seul appel par passe : la pénalité retient le reste"
    );
    assert!(comptes["spotify"]["reessayer_dans_s"].is_u64(), "{comptes}");
}

/// Un faux `/browse/featured-playlists` lent (80 ms) qui compte ses appels,
/// et un `/me` de même.
async fn fausse_selection(appels: Arc<AtomicUsize>) -> SpotifyService {
    let lent = move |_: HeaderMap| {
        let appels = appels.clone();
        async move {
            appels.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            axum::Json(
                json!({"playlists": {"items": [{"id": "p1", "name": "Sélection",
                "images": [], "tracks": {"total": 1}, "owner": {}}]}, "id": "moi"}),
            )
        }
    };
    let app = axum::Router::new()
        .route("/v1/browse/featured-playlists", get(lent.clone()))
        .route("/v1/me", get(lent));
    servir(app).await
}

/// Épreuve : deux affichages simultanés de la même rubrique, UN appel chez
/// Spotify, la même réponse pour les deux.
#[tokio::test]
async fn deux_demandes_simultanees_ne_font_qu_un_appel_8766() {
    let appels = Arc::new(AtomicUsize::new(0));
    let svc = fausse_selection(appels.clone()).await;
    let (a, b) = tokio::join!(svc.get_featured(), svc.get_featured());
    let (a, b) = (a.expect("sélection"), b.expect("sélection"));
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(
        appels.load(Ordering::SeqCst),
        1,
        "#8766 : la rubrique part deux fois vers Spotify (« ×2 »)"
    );
}

/// Contre-épreuve : rien n'est gardé après la réponse (un affichage
/// suivant redemande), et deux chemins différents restent deux appels.
#[tokio::test]
async fn rien_n_est_garde_et_deux_chemins_restent_deux_appels_8766() {
    let appels = Arc::new(AtomicUsize::new(0));
    let svc = fausse_selection(appels.clone()).await;
    svc.get_featured().await.expect("sélection");
    svc.get_featured().await.expect("sélection");
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "pas de cache après l'appel"
    );
    assert!(svc.en_vol.lock().unwrap().is_empty());

    let (a, b) = tokio::join!(
        svc.api_get_detaille("/browse/featured-playlists?limit=50"),
        svc.api_get_detaille("/me"),
    );
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(
        appels.load(Ordering::SeqCst),
        4,
        "deux chemins, deux appels"
    );
}
