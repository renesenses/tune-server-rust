//! #9486 (fils 3033 et 3053) — une catégorie (genre) que TIDAL ne propose
//! pas pour le pays du catalogue répond `404`, sous-statut `2001`
//! (« Category not found »). Elle s'affichait en erreur (502) dans la
//! sélection éditoriale et dans les widgets. Elle est désormais MASQUÉE :
//! ses albums répondent une liste vide (200 côté route), la liste des
//! genres l'omet ensuite, et le journal le note une fois.
//!
//! Un genre dont seuls les albums manquent mais qui a des playlists (#9119,
//! « Polish » avec `countryCode=PL`) reste dans la liste : sa page a de la
//! matière. Un autre refus de TIDAL (400…) reste une erreur.
//!
//! Faux TIDAL sur la boucle locale : aucun appel à TIDAL, aucun identifiant.
use super::*;
use crate::streaming::traits::StreamingService;

type Journal = Arc<std::sync::Mutex<Vec<String>>>;

async fn faux_tidal() -> (TidalService, Journal) {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    fn refus(statut: StatusCode, sous: u64, message: &str) -> axum::response::Response {
        (
            statut,
            axum::Json(serde_json::json!({
                "status": statut.as_u16(),
                "subStatus": sous,
                "userMessage": message,
            })),
        )
            .into_response()
    }
    async fn genres(State(journal): State<Journal>) -> axum::response::Response {
        journal.lock().unwrap().push("genres".into());
        axum::Json(serde_json::json!([
            {"name": "Pop", "path": "Pop"},
            {"name": "Absent", "path": "Absent"},
            {"name": "Polish", "path": "Polish"},
        ]))
        .into_response()
    }
    async fn genre(
        State(journal): State<Journal>,
        Path((genre, nature)): Path<(String, String)>,
    ) -> axum::response::Response {
        journal.lock().unwrap().push(format!("{genre}/{nature}"));
        match (genre.as_str(), nature.as_str()) {
            ("Pop", "albums") => axum::Json(serde_json::json!({
                "items": [{"id": 77, "title": "Un album", "artist": {"id": 1, "name": "X"}}]
            }))
            .into_response(),
            ("Absent", _) => refus(StatusCode::NOT_FOUND, 2001, "Category not found"),
            ("Polish", "albums") => refus(StatusCode::NOT_FOUND, 2001, "Category not found"),
            ("Polish", "playlists") => axum::Json(serde_json::json!({
                "items": [{"uuid": "pl-1", "title": "Polskie Hity", "creator": {"name": "TIDAL"}}]
            }))
            .into_response(),
            _ => refus(StatusCode::BAD_REQUEST, 1002, "Invalid request"),
        }
    }
    let journal: Journal = Arc::default();
    let app = axum::Router::new()
        .route("/genres", get(genres))
        .route("/genres/{genre}/{nature}", get(genre))
        .with_state(journal.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let svc = TidalService::pour_un_faux_tidal(format!("http://{adresse}"));
    (svc, journal)
}

fn noms(genres: &[StreamGenre]) -> Vec<&str> {
    genres.iter().map(|g| g.id.as_str()).collect()
}

#[tokio::test]
async fn une_categorie_absente_est_masquee_et_non_en_erreur_9486() {
    let (svc, _) = faux_tidal().await;
    let albums = svc
        .get_genre_albums("Absent", 50)
        .await
        .expect("une catégorie absente n'est pas une erreur (la route répond 200)");
    assert!(albums.is_empty());
    let genres = svc.get_genres(None).await.expect("liste des genres");
    assert_eq!(
        noms(&genres),
        ["Pop", "Polish"],
        "la liste des catégories omet celle que TIDAL ne propose pas"
    );
}

#[tokio::test]
async fn un_genre_sans_albums_mais_avec_playlists_reste_dans_la_liste_9486() {
    let (svc, _) = faux_tidal().await;
    assert!(svc.get_genre_albums("Polish", 50).await.unwrap().is_empty());
    let genres = svc.get_genres(None).await.unwrap();
    assert_eq!(noms(&genres), ["Pop", "Absent", "Polish"]);
}

#[tokio::test]
async fn un_autre_refus_reste_une_erreur_9486() {
    let (svc, _) = faux_tidal().await;
    match svc.get_genre_albums("Refus", 50).await {
        Err(TuneError::Streaming(m)) => assert!(m.contains("400"), "{m}"),
        autre => panic!("un 400 de TIDAL doit remonter, pas {autre:?}"),
    }
    assert_eq!(svc.get_genres(None).await.unwrap().len(), 3);
}

#[tokio::test]
async fn changer_de_pays_rend_la_categorie_9486() {
    let (mut svc, journal) = faux_tidal().await;
    svc.get_genre_albums("Absent", 50).await.unwrap();
    // Le refus n'est demandé qu'une fois : la seconde lecture est servie
    // sans rappeler TIDAL.
    svc.get_genre_albums("Absent", 50).await.unwrap();
    assert_eq!(
        journal
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.as_str() == "Absent/albums")
            .count(),
        1
    );
    svc.choisir_le_pays_du_catalogue(Some("FR")).unwrap();
    let genres = svc.get_genres(None).await.unwrap();
    assert_eq!(noms(&genres), ["Pop", "Absent", "Polish"]);
}
