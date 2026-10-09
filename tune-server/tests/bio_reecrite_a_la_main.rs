//! Une bio d'artiste réécrite à la main perd la provenance de l'ancienne.
//!
//! `ArtistRepo::update` écrit `bio` sans toucher `bio_source`,
//! `bio_source_url`, `bio_license` ni `bio_lang`. Un extrait Wikipédia
//! remplacé par le texte de l'utilisateur gardait donc son attribution, et le
//! client affichait « Source : Wikipédia — licence CC BY-SA 4.0 » sous un texte
//! qui n'en vient plus.
//!
//! Les deux routes d'édition sont couvertes : `PUT /library/artists/{id}` et
//! `POST /metadata/artists/{id}/edit`. Un texte inchangé (renvoyé avec un
//! autre champ) garde son attribution, qui reste juste.
//!
//! Hermétique : base en mémoire, aucun appel réseau (la bio stockée répond
//! avant le proxy communautaire).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;

const NOM: &str = "Miles Davis";
const EXTRAIT: &str = "Miles Davis est un trompettiste et compositeur de jazz américain.";
const URL: &str = "https://fr.wikipedia.org/wiki/Miles_Davis";

type Etat = tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// L'artiste 1, avec un extrait Wikipédia attribué.
fn artiste_avec_extrait(state: &Etat) -> ArtistRepo {
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&1i64 as &dyn ToSqlValue, &NOM as &dyn ToSqlValue],
        )
        .expect("insertion de l'artiste");
    let repo = ArtistRepo::with_backend(state.backend.clone());
    repo.update_bio_full(
        1,
        EXTRAIT,
        "wikipedia",
        Some(URL.to_string()),
        "CC BY-SA 4.0",
        "fr",
    )
    .expect("bio et provenance");
    assert!(
        repo.bio_provenance(1).unwrap().is_some(),
        "préalable : provenance posée"
    );
    repo
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(methode).uri(chemin);
    let req = match corps {
        Some(c) => req
            .header("content-type", "application/json")
            .body(Body::from(c.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn la_bio_reecrite_par_la_bibliotheque_perd_l_attribution() {
    let (app, state) = app_et_etat();
    let repo = artiste_avec_extrait(&state);

    let (status, _) = appeler(
        &app,
        "PUT",
        "/api/v1/library/artists/1",
        Some(json!({"bio": "Mon texte à moi sur Miles."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        repo.bio_provenance(1).unwrap(),
        None,
        "provenance effacée en base"
    );
    let (_, fiche) = appeler(&app, "GET", "/api/v1/library/artists/1", None).await;
    assert_eq!(fiche["bio"], "Mon texte à moi sur Miles.");
    assert!(fiche.get("bio_provenance").is_none(), "fiche : {fiche}");
    let (_, bio) = appeler(&app, "GET", "/api/v1/library/artists/1/bio?lang=fr", None).await;
    assert_eq!(bio["bio"], "Mon texte à moi sur Miles.");
    assert!(bio["bio_provenance"].is_null(), "route /bio : {bio}");
}

#[tokio::test]
async fn la_bio_reecrite_par_l_editeur_de_metadonnees_perd_l_attribution() {
    let (app, state) = app_et_etat();
    let repo = artiste_avec_extrait(&state);

    let (status, _) = appeler(
        &app,
        "POST",
        "/api/v1/metadata/artists/1/edit",
        Some(json!({"bio": "Mon texte à moi sur Miles."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(repo.bio_provenance(1).unwrap(), None);
    assert_eq!(
        repo.get(1).unwrap().unwrap().bio.as_deref(),
        Some("Mon texte à moi sur Miles.")
    );
}

#[tokio::test]
async fn la_bio_effacee_perd_aussi_l_attribution() {
    let (app, state) = app_et_etat();
    let repo = artiste_avec_extrait(&state);

    appeler(
        &app,
        "PUT",
        "/api/v1/library/artists/1",
        Some(json!({"bio": ""})),
    )
    .await;

    assert_eq!(repo.get(1).unwrap().unwrap().bio, None);
    assert_eq!(repo.bio_provenance(1).unwrap(), None);
}

/// Distinguant dans l'autre sens : effacer à chaque édition serait faux.
#[tokio::test]
async fn un_texte_inchange_garde_son_attribution() {
    let (app, state) = app_et_etat();
    let repo = artiste_avec_extrait(&state);

    // Le nom seul, puis la même bio renvoyée telle quelle.
    appeler(
        &app,
        "PUT",
        "/api/v1/library/artists/1",
        Some(json!({"name": "Miles Dewey Davis"})),
    )
    .await;
    appeler(
        &app,
        "PUT",
        "/api/v1/library/artists/1",
        Some(json!({"bio": EXTRAIT})),
    )
    .await;
    appeler(
        &app,
        "POST",
        "/api/v1/metadata/artists/1/edit",
        Some(json!({"bio": EXTRAIT})),
    )
    .await;

    let prov = repo.bio_provenance(1).unwrap().expect("provenance gardée");
    assert_eq!(prov["source"], "wikipedia");
    assert_eq!(prov["source_url"], URL);
    assert_eq!(prov["license"], "CC BY-SA 4.0");
    assert_eq!(prov["lang"], "fr");
}
