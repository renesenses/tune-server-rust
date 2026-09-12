//! #2373 (T4) — un partage de playlist se REPREND.
//!
//! `POST /playlists/{id}/share` écrivait `playlist_share_{id}` dans les
//! réglages et **aucune route ne l'effaçait**. Le lien donné une fois restait
//! lisible pour toujours : ni l'interface ni l'API n'avaient de quoi le
//! reprendre.
//!
//! ## Ce que ce fichier prouve, et comment
//!
//! Le parcours entier passe par le ROUTEUR, jamais par un gestionnaire appelé
//! à la main : partager → `200` sur le jeton → révoquer → `404` sur **le même
//! jeton**. C'est ce dernier `404` qui est le cœur de la tranche : sans lui,
//! rien ne prouve que la révocation révoque. Un essai qui s'arrêterait au
//! `204` du `DELETE` serait vert contre un handler qui ne fait rien.
//!
//! Le cloisonnement par profil suit le contrat #2794 : `404`, jamais `403`,
//! pour les deux nouvelles routes. Et le refus opposé au voisin est vérifié
//! **en base et sur le jeton** — un `404` poli posé devant un partage déjà
//! détruit se lirait comme une réussite.
//!
//! Le jeton n'est jamais imprimé : ni dans un message d'assertion, ni ailleurs.

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
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json)
}

/// Le partage public est délibérément hors profil : le jeton EST
/// l'autorisation. On l'appelle donc sans en-tête d'identité.
async fn lire_le_partage_public(app: &axum::Router, jeton: &str) -> (StatusCode, Value) {
    let req = Request::get(format!("/api/v1/playlists/shared/{jeton}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json)
}

/// La clé de partage telle qu'elle est réellement posée en base.
fn jeton_en_base(state: &tune_server::state::AppState, id: i64) -> Option<String> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("playlist_share_{id}"))
        .expect("lecture des reglages")
}

/// Crée une playlist appartenant au profil 1, garnie de deux pistes.
async fn playlist_du_profil_1(state: &tune_server::state::AppState, app: &axum::Router) -> i64 {
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
    assert_eq!(st, StatusCode::CREATED, "creation de la playlist d'essai");
    let id = body["id"].as_i64().expect("id playlist");
    let (st, _) = appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"track_ids": [t1, t2]})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "garnissage de la playlist d'essai");
    id
}

/// Partage la playlist et rend le jeton.
async fn partager(app: &axum::Router, id: i64) -> String {
    let (st, body) = appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "POST /playlists/{{id}}/share");
    body["token"]
        .as_str()
        .expect("un jeton de partage dans la reponse")
        .to_owned()
}

// --- le parcours entier ------------------------------------------------

/// Le cœur de la tranche. Sans le dernier `404`, rien ne prouve que la
/// révocation révoque.
#[tokio::test]
async fn revoquer_le_partage_rend_le_jeton_inutilisable() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;

    let jeton = partager(&app, id).await;

    // 1. Le lien marche.
    let (st, partage) = lire_le_partage_public(&app, &jeton).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "avant la revocation, le jeton doit ouvrir la playlist"
    );
    assert_eq!(partage["playlist"]["name"], "Privee du profil 1");
    assert_eq!(
        partage["tracks"].as_array().map(|a| a.len()),
        Some(2),
        "le partage doit rendre les deux pistes"
    );

    // 2. On le reprend.
    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NO_CONTENT,
        "DELETE /playlists/{{id}}/share doit repondre 204"
    );

    // 3. Le MEME jeton ne donne plus rien. C'est la seule preuve qui compte :
    //    un 204 poli devant un reglage jamais efface se lirait comme une
    //    reussite.
    let (st, corps) = lire_le_partage_public(&app, &jeton).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "le jeton revoque ouvre TOUJOURS la playlist : la revocation ne revoque rien"
    );
    assert!(
        corps.get("playlist").is_none(),
        "aucune playlist ne doit fuir dans le corps du refus"
    );

    // 4. Et la cle a bien disparu des reglages, pas seulement de la reponse.
    assert_eq!(
        jeton_en_base(&state, id),
        None,
        "le reglage playlist_share_{{id}} survit a la revocation"
    );
}

/// La playlist elle-même survit à la révocation : on retire le lien, pas la
/// playlist. Un `DELETE` qui supprimerait trop passerait l'essai précédent.
#[tokio::test]
async fn revoquer_le_partage_ne_supprime_pas_la_playlist() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;
    let _ = partager(&app, id).await;

    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, body) = appel(&app, "GET", &format!("/api/v1/playlists/{id}"), P1, None).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "la playlist doit survivre au retrait de son partage"
    );
    assert_eq!(body["name"], "Privee du profil 1");
    let (st, pistes) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        pistes.as_array().map(|a| a.len()),
        Some(2),
        "les pistes doivent survivre au retrait du partage"
    );
}

/// Retirer un partage qui n'existe pas est un non-événement, pas une erreur.
#[tokio::test]
async fn revoquer_un_partage_absent_est_idempotent() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;

    // Jamais partagee.
    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NO_CONTENT,
        "retirer un partage absent doit rendre 204, pas un refus"
    );

    // Deux fois de suite apres un vrai partage.
    let _ = partager(&app, id).await;
    for essai in 1..=2 {
        let (st, _) = appel(
            &app,
            "DELETE",
            &format!("/api/v1/playlists/{id}/share"),
            P1,
            None,
        )
        .await;
        assert_eq!(
            st,
            StatusCode::NO_CONTENT,
            "DELETE #{essai} doit rendre 204"
        );
    }
}

// --- l'etat du partage, pour l'ecran -----------------------------------

#[tokio::test]
async fn l_etat_du_partage_suit_le_partage_et_sa_reprise() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;

    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["shared"],
        json!(false),
        "une playlist jamais partagee ne doit pas se declarer partagee"
    );
    assert!(
        body.get("token").is_none(),
        "aucun jeton ne doit apparaitre pour une playlist non partagee"
    );

    let jeton = partager(&app, id).await;

    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["shared"],
        json!(true),
        "une playlist partagee doit se declarer partagee"
    );
    assert_eq!(
        body["token"].as_str(),
        Some(jeton.as_str()),
        "l'etat doit rendre le jeton en cours a son proprietaire"
    );
    assert_eq!(
        body["url"].as_str(),
        Some(format!("/api/v1/playlists/shared/{jeton}").as_str()),
        "l'etat doit rendre l'adresse publique du partage"
    );

    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, body) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["shared"],
        json!(false),
        "apres la reprise, l'etat doit redevenir « pas partagee »"
    );
    assert!(
        body.get("token").is_none(),
        "aucun jeton ne doit rester dans l'etat apres la reprise"
    );
}

// --- cloisonnement par profil : 404, jamais 403 (#2794) ----------------

#[tokio::test]
async fn revoquer_le_partage_d_un_autre_profil_est_refuse_sans_rien_detruire() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;
    let jeton = partager(&app, id).await;

    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/share"),
        P2,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "DELETE /playlists/{{id}}/share doit rendre 404 — jamais 403 (#2794)"
    );

    // La preuve se lit en base ET sur le jeton : un refus poli devant un
    // partage deja detruit serait un faux vert.
    assert!(
        jeton_en_base(&state, id).is_some(),
        "le partage a ete detruit malgre le refus oppose au voisin"
    );
    let (st, _) = lire_le_partage_public(&app, &jeton).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "le lien du proprietaire doit survivre au refus oppose au voisin"
    );

    // Temoin : le proprietaire, lui, revoque bel et bien. Sans lui, un
    // handler qui refuserait TOUT LE MONDE passerait cet essai.
    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = lire_le_partage_public(&app, &jeton).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lire_l_etat_de_partage_d_un_autre_profil_ne_rend_pas_le_jeton() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;
    let jeton = partager(&app, id).await;

    let (st, corps) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/share"),
        P2,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "GET /playlists/{{id}}/share doit rendre 404 — jamais 403 (#2794)"
    );
    let rendu = corps.to_string();
    assert!(
        !rendu.contains(&jeton),
        "le jeton de partage a fui dans la reponse opposee a un autre profil"
    );

    // Temoin : le proprietaire le lit.
    let (st, corps) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{id}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(corps["shared"], json!(true));
}

/// Une playlist qui n'existe pour personne rend le même `404` : le contrat ne
/// doit pas laisser distinguer « pas à vous » de « n'existe pas ».
#[tokio::test]
async fn les_deux_routes_rendent_404_sur_un_id_inexistant() {
    let state = etat();
    let app = appli(&state);
    let id = playlist_du_profil_1(&state, &app).await;
    let inexistant = id + 9_999;

    let (st, _) = appel(
        &app,
        "GET",
        &format!("/api/v1/playlists/{inexistant}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "GET sur un id inexistant");

    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{inexistant}/share"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "DELETE sur un id inexistant");
}
