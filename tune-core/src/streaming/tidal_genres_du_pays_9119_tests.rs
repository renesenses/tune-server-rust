//! #9119 — un genre ajouté par le pays du catalogue (« Polish » avec
//! `countryCode=PL`) s'ouvrait sur une page vide. Deux causes côté Tune :
//! le refus de TIDAL sur `/genres/{path}/albums` était lu comme une liste
//! vide (200 `[]`), et les playlists du genre n'étaient jamais demandées à
//! TIDAL. Un faux TIDAL sur la boucle locale : aucun appel à TIDAL, aucun
//! identifiant.
use super::*;
use crate::streaming::traits::StreamingService;

type Journal = Arc<std::sync::Mutex<Vec<String>>>;

async fn faux_tidal() -> (TidalService, Journal) {
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    async fn genre(
        State(journal): State<Journal>,
        Path((genre, nature)): Path<(String, String)>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        journal.lock().unwrap().push(format!(
            "{genre}/{nature}?countryCode={}",
            q.get("countryCode").cloned().unwrap_or_default()
        ));
        let refus = |statut: StatusCode, sous: u64| {
            (
                statut,
                axum::Json(serde_json::json!({
                    "status": statut.as_u16(),
                    "subStatus": sous,
                    "userMessage": "The requested resource could not be found",
                })),
            )
                .into_response()
        };
        match (genre.as_str(), nature.as_str()) {
            // Le genre local : pas d'albums, des playlists.
            ("Polish", "albums") => refus(StatusCode::NOT_FOUND, 2001),
            ("Polish", "playlists") => axum::Json(serde_json::json!({
                "limit": 50, "offset": 0, "totalNumberOfItems": 2,
                "items": [
                    {"uuid": "pl-1", "title": "Polskie Hity", "numberOfTracks": 40,
                     "creator": {"name": "TIDAL"}},
                    {"title": "sans identifiant"}
                ]
            }))
            .into_response(),
            ("Pop", "albums") => axum::Json(serde_json::json!({
                "items": [{"id": 77, "title": "Un album", "artist": {"id": 1, "name": "X"}}]
            }))
            .into_response(),
            ("Vide", "albums") => axum::Json(serde_json::json!({"items": []})).into_response(),
            _ => refus(StatusCode::BAD_REQUEST, 1002),
        }
    }
    let journal: Journal = Arc::default();
    let app = axum::Router::new()
        .route("/genres/{genre}/{nature}", get(genre))
        .with_state(journal.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let mut svc = TidalService::pour_un_faux_tidal(format!("http://{adresse}"));
    svc.choisir_le_pays_du_catalogue(Some("PL"))
        .expect("PL valide");
    (svc, journal)
}

#[tokio::test]
async fn un_genre_refuse_par_tidal_est_une_erreur_pas_une_page_vide_9119() {
    let (svc, journal) = faux_tidal().await;
    let resultat = svc.get_genre_albums("Polish", 50).await;
    match resultat {
        Err(TuneError::Streaming(m)) => assert!(m.contains("404"), "statut de TIDAL : {m}"),
        autre => panic!("le refus de TIDAL doit remonter, pas {autre:?}"),
    }
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["Polish/albums?countryCode=PL"],
        "le pays du catalogue part dans la requête"
    );
}

#[tokio::test]
async fn une_liste_vide_reste_une_liste_vide_et_les_albums_passent_9119() {
    let (svc, _) = faux_tidal().await;
    assert!(
        svc.get_genre_albums("Vide", 50)
            .await
            .expect("200 vide")
            .is_empty()
    );
    let albums = svc.get_genre_albums("Pop", 50).await.expect("200");
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].title, "Un album");
}

#[tokio::test]
async fn les_playlists_du_genre_viennent_de_tidal_9119() {
    let (svc, journal) = faux_tidal().await;
    let playlists = svc
        .get_featured_playlists(None, Some("Polish"))
        .await
        .expect("playlists du genre");
    let noms: Vec<_> = playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        noms,
        ["Polskie Hity"],
        "l'entrée sans identifiant est écartée"
    );
    assert_eq!(playlists[0].id, "pl-1");
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["Polish/playlists?countryCode=PL"]
    );
}

#[tokio::test]
async fn un_refus_sur_les_playlists_laisse_la_bande_absente_9119() {
    let (svc, journal) = faux_tidal().await;
    let playlists = svc
        .get_featured_playlists(None, Some("Vide"))
        .await
        .expect("un refus n'est pas une panne de l'écran");
    assert!(playlists.is_empty());
    assert_eq!(journal.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn sans_genre_ou_avec_un_tag_rien_ne_part_9119() {
    let (svc, journal) = faux_tidal().await;
    for (tag, genre) in [
        (None, None),
        (Some("moods"), Some("Polish")),
        (None, Some("")),
    ] {
        assert!(
            svc.get_featured_playlists(tag, genre)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(journal.lock().unwrap().is_empty(), "aucun appel à TIDAL");
}

#[test]
fn refus_de_tidal_ne_prend_que_les_corps_d_erreur_9119() {
    use serde_json::json;
    assert_eq!(
        TidalService::refus_de_tidal(
            &json!({"status": 404, "subStatus": 2001, "userMessage": "absent"})
        ),
        Some("tidal 404 (2001): absent".into())
    );
    // Une vraie liste, même vide, n'est pas un refus.
    assert_eq!(TidalService::refus_de_tidal(&json!({"items": []})), None);
    // Un objet qui porte `status` ET une liste reste une liste.
    assert_eq!(
        TidalService::refus_de_tidal(&json!({"status": 404, "items": []})),
        None
    );
    // Un statut de succès n'est pas un refus.
    assert_eq!(TidalService::refus_de_tidal(&json!({"status": 200})), None);
}
