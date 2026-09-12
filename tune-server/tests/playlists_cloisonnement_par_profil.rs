//! #2794 — une playlist ne s'atteint pas par son id depuis un autre profil.
//!
//! Le cloisonnement ne tenait qu'au listing (`GET /playlists`), au comptage et
//! à la création. Tous les accès **par id** — lecture, renommage, suppression,
//! pistes, duplication, export, partage, transfert vers une file, diff —
//! partaient d'un `WHERE id = ?` nu. Les ids de playlists sont de petits
//! entiers séquentiels : les énumérer donnait la bibliothèque de playlists de
//! tout le foyer.
//!
//! ## Ce que ce fichier prouve, et comment
//!
//! Deux profils réels dans la base ; l'identité de l'appelant est portée par
//! l'en-tête `X-Profile-Id`, que l'extracteur `ActiveProfile` honore (auth
//! désactivée = LAN de confiance, le profil est le seul « qui »). Chaque
//! écriture refusée est **vérifiée en base**, jamais sur le code de retour :
//! c'est le piège du « 200 pour rien » retourné — ici ce serait un « 404 pour
//! rien », une réponse polie posée devant une base déjà modifiée.
//!
//! Le témoin anti-régression tient dans les mêmes essais : chaque refus opposé
//! au profil 2 est doublé du même appel par le profil 1, qui doit réussir.
//! Sans lui, un handler qui répondrait 404 à tout le monde passerait le test.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

// --- outillage ---------------------------------------------------------

const P1: &str = "1";
const P2: &str = "2";

fn etat() -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le profil visé par `X-Profile-Id` doit exister, sinon l'extracteur
    // retombe sur le profil actif global et les deux « utilisateurs » de
    // l'essai seraient le même.
    let profils = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone());
    let id = profils
        .create("voisin", Some("Le voisin"), None)
        .expect("create profile");
    assert_eq!(id, 2, "le second profil doit porter l'id 2");
    state
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

fn piste(state: &tune_server::state::AppState, titre: &str, chemin: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(chemin.into());
    repo.create(&t).expect("insert track")
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    profil: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", profil);
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    reponse(app, req.body(body).unwrap()).await
}

/// Réponse brute : l'export ne rend pas du JSON.
async fn appel_brut(app: &axum::Router, path: &str, profil: &str) -> (StatusCode, String) {
    let req = Request::get(path)
        .header("X-Profile-Id", profil)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// --- lectures en base (la seule preuve qui compte) ---------------------

fn nom_en_base(state: &tune_server::state::AppState, id: i64) -> Option<String> {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .get(id)
        .expect("get playlist")
        .map(|p| p.name)
}

fn pistes_en_base(state: &tune_server::state::AppState, id: i64) -> Vec<i64> {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .get_track_ids(id)
        .expect("get_track_ids")
}

/// Crée une playlist appartenant au profil 1, garnie de deux pistes.
async fn playlist_du_profil_1(
    state: &tune_server::state::AppState,
    app: &axum::Router,
) -> (i64, i64, i64) {
    let t1 = piste(state, "Piste Un", "/musique/un.flac");
    let t2 = piste(state, "Piste Deux", "/musique/deux.flac");
    let (st, body) = appel(
        app,
        "POST",
        "/api/v1/playlists",
        P1,
        Some(json!({"name": "Privee du profil 1"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = body["id"].as_i64().expect("id playlist");
    let (st, _) = appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"track_ids": [t1, t2]})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    (id, t1, t2)
}

// --- lecture -----------------------------------------------------------

#[tokio::test]
async fn lire_la_playlist_d_un_autre_profil_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let (st, _) = appel(&app, "GET", &format!("/api/v1/playlists/{id}"), P2, None).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "GET /playlists/{{id}} doit ignorer la playlist d'un autre profil"
    );

    // Témoin : le propriétaire, lui, la lit.
    let (st, body) = appel(&app, "GET", &format!("/api/v1/playlists/{id}"), P1, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["name"], "Privee du profil 1");
}

#[tokio::test]
async fn lister_les_pistes_d_un_autre_profil_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/tracks"),
        P2,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(
        body.get("tracks").is_none() && !body.is_array(),
        "aucune piste ne doit fuir dans le corps : {body}"
    );

    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body.as_array().map(|a| a.len()), Some(2));
}

// --- écritures : la preuve se lit EN BASE ------------------------------

#[tokio::test]
async fn renommer_la_playlist_d_un_autre_profil_ne_touche_pas_la_base() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let (st, _) = appel(
        &app,
        "PUT",
        &format!("/api/v1/playlists/{id}"),
        P2,
        Some(json!({"name": "Detournee"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        nom_en_base(&state, id).as_deref(),
        Some("Privee du profil 1"),
        "le nom a changé en base malgré le refus"
    );

    // Témoin : le propriétaire renomme bel et bien.
    let (st, _) = appel(
        &app,
        "PUT",
        &format!("/api/v1/playlists/{id}"),
        P1,
        Some(json!({"name": "Renommee"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(nom_en_base(&state, id).as_deref(), Some("Renommee"));
}

#[tokio::test]
async fn supprimer_la_playlist_d_un_autre_profil_ne_touche_pas_la_base() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let (st, _) = appel(&app, "DELETE", &format!("/api/v1/playlists/{id}"), P2, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(
        nom_en_base(&state, id).is_some(),
        "la playlist a été supprimée malgré le refus"
    );

    let (st, _) = appel(&app, "DELETE", &format!("/api/v1/playlists/{id}"), P1, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(nom_en_base(&state, id).is_none());
}

#[tokio::test]
async fn muter_les_pistes_d_un_autre_profil_ne_touche_pas_la_base() {
    let state = etat();
    let app = appli(&state);
    let (id, t1, t2) = playlist_du_profil_1(&state, &app).await;
    let intrus = piste(&state, "Intruse", "/musique/intruse.flac");
    let attendu = vec![t1, t2];

    // ajout
    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        P2,
        Some(json!({"track_ids": [intrus]})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        pistes_en_base(&state, id),
        attendu,
        "ajout passé quand même"
    );

    // retrait par position
    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks/remove"),
        P2,
        Some(json!({"position": 0})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        pistes_en_base(&state, id),
        attendu,
        "retrait passé quand même"
    );

    // retrait en lot
    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/tracks"),
        P2,
        Some(json!({"positions": [0, 1]})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        pistes_en_base(&state, id),
        attendu,
        "retrait en lot passé quand même"
    );

    // réordonnancement
    let (st, _) = appel(
        &app,
        "PUT",
        &format!("/api/v1/playlists/{id}/tracks"),
        P2,
        Some(json!({"track_ids": [t2, t1]})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        pistes_en_base(&state, id),
        attendu,
        "réordonnancement passé quand même"
    );

    // Témoin : le propriétaire réordonne pour de bon.
    let (st, _) = appel(
        &app,
        "PUT",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"track_ids": [t2, t1]})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(pistes_en_base(&state, id), vec![t2, t1]);
}

// --- duplication, export, partage --------------------------------------

#[tokio::test]
async fn dupliquer_la_playlist_d_un_autre_profil_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/duplicate"),
        P2,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Une copie créée sous le profil 2 serait une exfiltration : le listing du
    // profil 2 doit rester vide.
    let (_, liste) = appel(&app, "GET", "/api/v1/playlists/all", P2, None).await;
    assert_eq!(
        liste.as_array().map(|a| a.len()),
        Some(0),
        "une copie a été créée sous le profil 2 : {liste}"
    );

    // Témoin : le propriétaire duplique.
    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/duplicate"),
        P1,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
}

#[tokio::test]
async fn exporter_la_playlist_d_un_autre_profil_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    for suffixe in ["", "?format=json", "?format=csv", "?format=xspf"] {
        let (st, corps) =
            appel_brut(&app, &format!("/api/v1/playlists/{id}/export{suffixe}"), P2).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "export{suffixe} non cloisonné");
        assert!(
            !corps.contains("Piste Un"),
            "l'export{suffixe} a laissé fuir une piste : {corps}"
        );
    }

    // Témoin : le propriétaire exporte, et le contenu est bien là.
    let (st, corps) = appel_brut(&app, &format!("/api/v1/playlists/{id}/export"), P1).await;
    assert_eq!(st, StatusCode::OK);
    assert!(corps.contains("Piste Un"), "export du propriétaire vide");
}

#[tokio::test]
async fn partager_la_playlist_d_un_autre_profil_ne_cree_aucun_jeton() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/share"),
        P2,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Preuve en base : aucun jeton n'a été écrit. Un jeton publié survivrait à
    // la session et rendrait la playlist lisible sans aucune identité.
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    assert!(
        reglages
            .get(&format!("playlist_share_{id}"))
            .expect("lecture reglage")
            .is_none(),
        "un jeton de partage a été écrit malgré le refus"
    );

    // Témoin : le propriétaire partage, et le jeton devient utilisable.
    let (st, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let jeton = body["token"].as_str().expect("jeton").to_string();

    // Le partage public par jeton reste délibérément hors profil : le jeton EST
    // l'autorisation. Le profil 2 y accède, comme n'importe qui l'ayant reçu.
    let (st, partage) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/shared/{jeton}"),
        P2,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "le partage par jeton doit rester accessible"
    );
    assert_eq!(partage["playlist"]["name"], "Privee du profil 1");
}

// --- transfert et diff --------------------------------------------------

#[tokio::test]
async fn transferer_la_playlist_d_un_autre_profil_ne_remplit_aucune_file() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = zones.create("Salon", Some("local"), None).expect("zone");

    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/playlists/transfer",
        P2,
        Some(json!({"playlist_id": id, "zone_id": zone})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Preuve en base : la file de la zone est restée vide.
    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(state.backend.clone());
    let restant = file.count_all(zone).expect("lecture file");
    assert_eq!(
        restant, 0,
        "les pistes d'un autre profil sont arrivées dans la file"
    );

    // Témoin : le propriétaire transfère, et la file se remplit.
    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/playlists/transfer",
        P1,
        Some(json!({"playlist_id": id, "zone_id": zone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(file.count_all(zone).expect("lecture file"), 2);
}

#[tokio::test]
async fn le_diff_ne_revele_pas_les_pistes_d_un_autre_profil() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    // Le profil 2 compare la playlist du profil 1 avec une liste vide : sans
    // cloisonnement, `only_in_source` en récite titres et artistes.
    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlists/diff",
        P2,
        Some(json!({
            "source_service": "local",
            "source_playlist_id": id.to_string(),
            "target_service": "local",
            "target_playlist_id": "999999",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["only_in_source"].as_array().map(|a| a.len()),
        Some(0),
        "le diff a récité la playlist d'un autre profil : {body}"
    );
    assert!(
        !body.to_string().contains("Piste Un"),
        "un titre a fuité par le diff : {body}"
    );

    // Témoin : pour le propriétaire, le diff dit bien ce qu'il contient.
    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlists/diff",
        P1,
        Some(json!({
            "source_service": "local",
            "source_playlist_id": id.to_string(),
            "target_service": "local",
            "target_playlist_id": "999999",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["only_in_source"].as_array().map(|a| a.len()), Some(2));
}

// --- résolution par une étiquette --------------------------------------

/// Les étiquettes sont communes au foyer : `GET /tags/{id}/playlists`
/// résolvait le nom et le nombre de pistes de playlists appartenant à
/// d'autres profils, sans jamais passer par `/playlists`.
#[tokio::test]
async fn une_etiquette_ne_resout_pas_les_playlists_d_un_autre_profil() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let tags = tune_core::db::tag_repo::TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Rituels", None).expect("create tag");
    tags.tag_item(tag, "playlist", id).expect("tag playlist");

    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/tags/{tag}/playlists"),
        P2,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["count"], 0,
        "l'étiquette a résolu la playlist d'un autre profil : {body}"
    );

    // Témoin : pour le propriétaire, l'étiquette résout bien sa playlist.
    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/tags/{tag}/playlists"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], 1);
    assert_eq!(body["playlists"][0]["name"], "Privee du profil 1");
}

// --- disponibilité ------------------------------------------------------

#[tokio::test]
async fn verifier_la_disponibilite_d_un_autre_profil_est_refuse() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    for route in ["recover", "recover/apply"] {
        let (st, body) = appel(
            &app,
            "POST",
            &format!("/api/v1/playlists/{id}/{route}"),
            P2,
            Some(json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "/{route} non cloisonné");
        assert!(
            !body.to_string().contains("Piste Un"),
            "/{route} a laissé fuir une piste : {body}"
        );
    }

    // Témoin : le propriétaire obtient bien son bilan.
    let (st, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/recover"),
        P1,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["total_tracks"], 2);
}
