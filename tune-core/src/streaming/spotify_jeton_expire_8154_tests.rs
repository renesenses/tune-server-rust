//! #8154 (fil 2288) — « token expired » en pleine lecture : le pilote de
//! production renouvelle le jeton une fois et refait l'appel une fois.
//!
//! Un faux Spotify local (axum sur 127.0.0.1) tient `/api/token` et l'API Web
//! (`/v1/tracks/{id}`, `/v1/me/player/devices`, `/v1/me/player/play`). Il
//! n'accepte que le jeton qu'on lui désigne et compte chaque appel. Aucun
//! compte Spotify, aucun librespot, aucun son.

use super::*;
use crate::streaming::StreamingService;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Compteurs {
    jeton: AtomicUsize,
    titre: AtomicUsize,
    appareils: AtomicUsize,
    lecture: AtomicUsize,
}

struct FauxSpotify {
    compteurs: Arc<Compteurs>,
    pilote: PiloteDuRegistre,
}

/// `jeton_initial` : celui que Tune détient. `/api/token` rend `renouvele`
/// (ou un refus définitif si `None`). L'API Web n'accepte que `accepte`.
async fn faux_spotify(jeton_initial: &str, renouvele: Option<&str>, accepte: &str) -> FauxSpotify {
    let compteurs = Arc::new(Compteurs::default());
    let accepte = format!("Bearer {accepte}");
    let autorise = move |entetes: &HeaderMap| {
        entetes.get("authorization").and_then(|v| v.to_str().ok()) == Some(accepte.as_str())
    };
    let refus = || {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"status": 401, "message": "The access token expired"}})),
        )
    };
    let renouvele = renouvele.map(str::to_owned);
    let app = axum::Router::new()
        .route(
            "/api/token",
            post({
                let c = compteurs.clone();
                move || {
                    c.jeton.fetch_add(1, Ordering::SeqCst);
                    let renouvele = renouvele.clone();
                    async move {
                        match renouvele {
                            Some(j) => (
                                StatusCode::OK,
                                axum::Json(json!({"access_token": j, "expires_in": 3600})),
                            ),
                            None => (
                                StatusCode::BAD_REQUEST,
                                axum::Json(json!({"error": "invalid_grant"})),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/v1/tracks/{id}",
            get({
                let c = compteurs.clone();
                let autorise = autorise.clone();
                move |entetes: HeaderMap| {
                    c.titre.fetch_add(1, Ordering::SeqCst);
                    let ok = autorise(&entetes);
                    async move {
                        if ok {
                            (
                                StatusCode::OK,
                                axum::Json(
                                    json!({"id": "piste", "name": "Essai", "duration_ms": 180000}),
                                ),
                            )
                        } else {
                            refus()
                        }
                    }
                }
            }),
        )
        .route(
            "/v1/me/player/devices",
            get({
                let c = compteurs.clone();
                let autorise = autorise.clone();
                move |entetes: HeaderMap| {
                    c.appareils.fetch_add(1, Ordering::SeqCst);
                    let ok = autorise(&entetes);
                    async move {
                        if ok {
                            (
                                StatusCode::OK,
                                axum::Json(
                                    json!({"devices": [{"id": "appareil-1", "name": "Tune"}]}),
                                ),
                            )
                        } else {
                            refus()
                        }
                    }
                }
            }),
        )
        .route(
            "/v1/me/player/play",
            put({
                let c = compteurs.clone();
                move |entetes: HeaderMap| {
                    c.lecture.fetch_add(1, Ordering::SeqCst);
                    let ok = autorise(&entetes);
                    async move {
                        if ok {
                            StatusCode::NO_CONTENT.into_response_pair()
                        } else {
                            refus().into_response_pair()
                        }
                    }
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });

    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    assert!(svc.restore_tokens(&json!({
        "access_token": jeton_initial,
        "refresh_token": "renouvellement",
    })));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(svc));
    FauxSpotify {
        compteurs,
        pilote: PiloteDuRegistre::new(Arc::new(Mutex::new(registre))),
    }
}

/// Une réponse vide (204) ou un refus JSON, sous un même type.
trait EnReponse {
    fn into_response_pair(self) -> axum::response::Response;
}
impl EnReponse for StatusCode {
    fn into_response_pair(self) -> axum::response::Response {
        axum::response::IntoResponse::into_response(self)
    }
}
impl EnReponse for (StatusCode, axum::Json<serde_json::Value>) {
    fn into_response_pair(self) -> axum::response::Response {
        axum::response::IntoResponse::into_response(self)
    }
}

fn n(c: &AtomicUsize) -> usize {
    c.load(Ordering::SeqCst)
}

/// Épreuve : le jeton détenu est refusé, le renouvelé est accepté. La
/// lecture part, dans le même geste, avec le nouveau jeton.
#[tokio::test]
async fn lancer_renouvelle_le_jeton_refuse_et_reessaie_une_fois_8154() {
    let f = faux_spotify("jeton-perime", Some("jeton-neuf"), "jeton-neuf").await;
    let issue = f.pilote.lancer("Tune", "piste", 0).await;
    assert_eq!(
        issue,
        Ok(()),
        "#8154 : la lecture doit partir après le renouvellement"
    );
    assert_eq!(n(&f.compteurs.jeton), 1, "un seul renouvellement");
    assert_eq!(
        n(&f.compteurs.appareils),
        2,
        "un refus, puis un seul nouvel essai"
    );
    assert_eq!(n(&f.compteurs.lecture), 1);
}

/// Épreuve : même chose sur la durée du titre (`GET /tracks/{id}`), premier
/// appel de la lecture à l'API Web.
#[tokio::test]
async fn duree_renouvelle_le_jeton_refuse_et_reessaie_une_fois_8154() {
    let f = faux_spotify("jeton-perime", Some("jeton-neuf"), "jeton-neuf").await;
    assert_eq!(f.pilote.duree_ms("piste").await, Ok(180000));
    assert_eq!(n(&f.compteurs.jeton), 1);
    assert_eq!(n(&f.compteurs.titre), 2);
}

/// Contre-épreuve : le jeton renouvelé est refusé lui aussi. Un seul
/// renouvellement, un seul nouvel essai (pas de boucle), puis le message
/// d'action, jamais « token expired ».
#[tokio::test]
async fn jeton_encore_refuse_rend_le_message_d_action_sans_boucler_8154() {
    let f = faux_spotify("jeton-perime", Some("jeton-encore-refuse"), "jamais").await;
    let issue = f.pilote.lancer("Tune", "piste", 0).await;
    assert_eq!(issue, Err(MOTIF_JETON_REFUSE.to_string()));
    assert_eq!(n(&f.compteurs.jeton), 1, "pas de deuxième renouvellement");
    assert_eq!(n(&f.compteurs.appareils), 2, "pas de deuxième nouvel essai");
    assert_eq!(n(&f.compteurs.lecture), 0);

    let duree = f.pilote.duree_ms("piste").await;
    assert_eq!(duree, Err(MOTIF_JETON_REFUSE.to_string()));
    assert_eq!(n(&f.compteurs.titre), 2);
}

/// Contre-épreuve : Spotify refuse le renouvellement (`invalid_grant`).
/// Aucun nouvel essai, le message d'action.
#[tokio::test]
async fn renouvellement_refuse_rend_le_message_d_action_8154() {
    let f = faux_spotify("jeton-perime", None, "jeton-neuf").await;
    let issue = f.pilote.lancer("Tune", "piste", 0).await;
    assert_eq!(issue, Err(MOTIF_JETON_REFUSE.to_string()));
    assert_eq!(n(&f.compteurs.jeton), 1);
    assert_eq!(
        n(&f.compteurs.appareils),
        1,
        "rien à réessayer sans jeton neuf"
    );
}

/// Contre-épreuve : un jeton accepté ne déclenche aucun renouvellement.
#[tokio::test]
async fn jeton_accepte_ne_renouvelle_rien_8154() {
    let f = faux_spotify("jeton-neuf", Some("autre"), "jeton-neuf").await;
    assert_eq!(f.pilote.lancer("Tune", "piste", 0).await, Ok(()));
    assert_eq!(f.pilote.duree_ms("piste").await, Ok(180000));
    assert_eq!(n(&f.compteurs.jeton), 0);
    assert_eq!(n(&f.compteurs.appareils), 1);
    assert_eq!(n(&f.compteurs.titre), 1);
}
