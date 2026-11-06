//! #7659 — la recherche Spotify ne rendait rien.
//!
//! Doublure locale de `/v1/search` : comme Spotify, elle refuse par
//! `400 Invalid limit` toute `limit` au-dessus du plafond de l'application,
//! et `limit=0`. Aucune requête ne part vers Spotify.

use super::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Doublure {
    /// Le plus grand `limit` accepté : 50 en quota étendu, 10 en mode
    /// développement.
    plafond: usize,
    /// Les `(limit, offset)` reçus, dans l'ordre.
    recues: Arc<Mutex<Vec<(usize, usize)>>>,
}

fn element(cat: &str, i: usize) -> serde_json::Value {
    json!({
        "id": format!("{cat}{i}"),
        "name": format!("Miles Davis {cat} {i}"),
        "artists": [{"id": "a0", "name": "Miles Davis"}],
        "album": {"id": "al0", "name": "Kind of Blue"},
    })
}

async fn recherche_double(
    axum::extract::State(d): axum::extract::State<Doublure>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let limit: usize = q["limit"].parse().unwrap();
    let offset: usize = q.get("offset").map_or(0, |o| o.parse().unwrap());
    d.recues.lock().unwrap().push((limit, offset));
    if q["q"] == "refus" {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(
                json!({"error": {"status": 403, "message": "Check settings on developer.spotify.com/dashboard, the user may not be registered."}}),
            ),
        );
    }
    if limit == 0 || limit > d.plafond {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"status": 400, "message": "Invalid limit"}})),
        );
    }
    // Un catalogue de 25 éléments par catégorie.
    let total = 25;
    let page = |cat: &str| {
        let items: Vec<_> = (offset..(offset + limit).min(total))
            .map(|i| element(cat, i))
            .collect();
        let next = (offset + limit < total).then_some("https://api.spotify.com/v1/search?next");
        json!({"items": items, "total": total, "offset": offset, "limit": limit, "next": next})
    };
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"tracks": page("t"), "albums": page("al"), "artists": page("ar")})),
    )
}

async fn service_double(plafond: usize) -> (SpotifyService, Doublure) {
    let d = Doublure {
        plafond,
        recues: Arc::default(),
    };
    let app = axum::Router::new()
        .route("/v1/search", axum::routing::get(recherche_double))
        .with_state(d.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    (svc, d)
}

#[tokio::test]
async fn recherche_a_la_limite_du_client_web_rend_des_resultats_7659() {
    // Le client web demande 100 par catégorie ; Spotify n'en accepte que 50.
    let (svc, d) = service_double(50).await;
    let r = svc.search("miles davis", 100).await.expect("recherche");
    assert_eq!(
        (r.tracks.len(), r.albums.len(), r.artists.len()),
        (25, 25, 25),
        "#7659 : la recherche Spotify à limit=100 doit rendre des résultats ; requêtes reçues : {:?}",
        d.recues.lock().unwrap()
    );
    assert_eq!(d.recues.lock().unwrap().as_slice(), &[(50, 0)]);
}

#[tokio::test]
async fn recherche_tous_n_envoie_pas_limit_zero_7659() {
    let (svc, d) = service_double(50).await;
    let r = svc.search("miles davis", 0).await.expect("recherche");
    assert!(
        !r.tracks.is_empty(),
        "#7659 : « Tous » (limit=0) doit rendre des pistes"
    );
    assert_eq!(d.recues.lock().unwrap().as_slice(), &[(50, 0)]);
}

#[tokio::test]
async fn application_en_mode_developpement_adopte_le_plafond_de_10_7659() {
    let (svc, d) = service_double(10).await;
    let r = svc.search("miles davis", 20).await.expect("recherche");
    assert_eq!(
        r.tracks.len(),
        10,
        "#7659 : en mode développement (plafond 10), limit=20 doit rendre 10 pistes"
    );
    // Le plafond appris vaut pour la suite : plus de requête refusée.
    assert_eq!(svc.limite_de_page_recherche(100), 10);
    svc.search("coltrane", 20).await.expect("seconde recherche");
    assert_eq!(
        d.recues.lock().unwrap().as_slice(),
        &[(20, 0), (10, 0), (10, 0)]
    );
}

#[tokio::test]
async fn refus_de_spotify_ressort_en_erreur_et_non_en_liste_vide_7659() {
    let (svc, _) = service_double(50).await;
    let erreur = svc
        .search("refus", 20)
        .await
        .expect_err("#7659 : un refus de Spotify ne doit pas passer pour « aucun résultat »")
        .to_string();
    assert!(erreur.contains("403"), "{erreur}");
    assert!(erreur.contains("user may not be registered"), "{erreur}");
}

#[tokio::test]
async fn la_page_suivante_part_de_l_offset_demande_7659() {
    let (svc, d) = service_double(10).await;
    let premiere = svc.search_page("miles davis", 10, 0).await.expect("page 1");
    assert!(premiere.has_more);
    assert_eq!(premiere.totals.tracks, 25);
    let seconde = svc
        .search_page("miles davis", 10, 20)
        .await
        .expect("page 3");
    assert_eq!(seconde.results.tracks.len(), 5);
    assert_eq!(seconde.results.tracks[0].id, "t20");
    assert!(!seconde.has_more);
    assert_eq!(d.recues.lock().unwrap().as_slice(), &[(10, 0), (10, 20)]);
}

/// Doublure de `/me/player/devices` : `refus` réponses 429 d'abord, puis
/// l'appareil `Tune` ; `/me/player/play` accepte.
async fn service_appareils(refus: usize) -> (SpotifyService, Arc<Mutex<usize>>) {
    let appels: Arc<Mutex<usize>> = Arc::default();
    let app = axum::Router::new()
        .route(
            "/v1/me/player/devices",
            axum::routing::get(
                move |axum::extract::State(n): axum::extract::State<Arc<Mutex<usize>>>| async move {
                    let mut n = n.lock().unwrap();
                    *n += 1;
                    if *n <= refus {
                        return (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(json!({"error": {"status": 429, "message": "API rate limit exceeded"}})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({"devices": [{"id": "dev-tune", "name": "Tune"}]})),
                    )
                },
            ),
        )
        .route(
            "/v1/me/player/play",
            axum::routing::put(|| async { axum::http::StatusCode::NO_CONTENT }),
        )
        .with_state(appels.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    (svc, appels)
}

#[tokio::test]
async fn un_refus_passager_n_arrete_pas_l_attente_de_l_appareil_7659() {
    // Le contrôle de statut d'`api_get` ne doit pas fragiliser la lecture :
    // un 429 pendant l'attente de l'appareil librespot se réessaie.
    let (svc, appels) = service_appareils(1).await;
    svc.lancer_sur_l_appareil("Tune", "x", 0)
        .await
        .expect("#7659 : un 429 de /me/player/devices doit être réessayé, pas arrêter la lecture");
    assert_eq!(*appels.lock().unwrap(), 2);
}

#[tokio::test]
async fn un_refus_persistant_donne_le_message_de_spotify_7659() {
    let (svc, appels) = service_appareils(usize::MAX).await;
    let erreur = svc
        .lancer_sur_l_appareil("Tune", "x", 0)
        .await
        .expect_err("refus à chaque essai");
    assert!(erreur.contains("429"), "{erreur}");
    assert_eq!(*appels.lock().unwrap(), 20);
}
