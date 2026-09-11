//! Le bouton « + Ajouter à Tune » ne pose pas un DOUBLON quand le catalogue
//! porte déjà le flux (#3543).
//!
//! Mesure du 09/09/2026 sur `GET https://mozaiklabs.fr/api/v1/radios` :
//! l'annuaire y sert **dix** entrées Radio Paradise pour **sept** canaux. Trois
//! adresses de flux y figurent DEUX fois, sous deux noms différents :
//!
//! ```text
//! http://stream.radioparadise.com/flacm         id 15 « Radio Paradise - Main Mix »   id 67 « Radio paradise - Main Mix »
//! http://stream.radioparadise.com/rock-flacm    id 56 « Radio Paradise Rock Mix »     id 69 « Radio Paradise - Rock »
//! http://stream.radioparadise.com/mellow-flacm  id 66 « Radio Paradise »              id 68 « Radio Paradise - Mellow Mix »
//! ```
//!
//! Chaque entrée de cette page porte son bouton, et chaque bouton tombe sur
//! `GET /api/v1/radios/add`. Le testeur venu signaler des doublons dans
//! l'annuaire se les posait donc lui-même dans SA liste, en deux clics — et
//! `radio_stations` n'a aucune contrainte d'unicité pour l'en empêcher.
//!
//! Ce que ces essais tiennent :
//!   1. deux ajouts de la MÊME adresse laissent UNE station — et la remettent
//!      en favori, parce que c'est ce que le geste demandait ;
//!   2. contre-épreuve : une adresse DIFFÉRENTE sur le même diffuseur crée
//!      bien une seconde station. Une porte qui refuserait tout passerait le
//!      premier essai.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::db::radio_repo::RadioRepo;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// Un clic sur « + Ajouter à Tune », par sa VRAIE route.
async fn ajouter_depuis_le_web(app: &axum::Router, nom: &str, url: &str) -> (StatusCode, String) {
    let chemin = format!(
        "/api/v1/radios/add?name={}&url={}",
        urlencoding(nom),
        urlencoding(url)
    );
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&octets).to_string())
}

/// Échappement de requête minimal — assez pour les deux caractères qui gênent
/// ici (`:` et `/` passent, l'espace et le `&` non).
fn urlencoding(brut: &str) -> String {
    brut.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '&' => "%26".to_string(),
            '?' => "%3F".to_string(),
            '=' => "%3D".to_string(),
            '#' => "%23".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// Les stations dont l'adresse est exactement celle-ci.
fn stations_sur(state: &tune_server::state::AppState, url: &str) -> Vec<(i64, String, bool)> {
    RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .into_iter()
        .filter(|s| s.url == url)
        .map(|s| (s.id.unwrap_or_default(), s.name, s.is_favorite))
        .collect()
}

const FLUX: &str = "http://stream.exemple-radio.test/flacm";

#[tokio::test]
async fn deux_entrees_d_annuaire_sur_le_meme_flux_ne_posent_qu_une_station() {
    let (app, state) = app_et_etat();

    // Le bouton de la première entrée d'annuaire.
    let (status, _) = ajouter_depuis_le_web(&app, "Exemple - Main Mix", FLUX).await;
    assert_eq!(status, StatusCode::OK, "le premier ajout doit aboutir");
    let apres_un = stations_sur(&state, FLUX);
    assert_eq!(
        apres_un.len(),
        1,
        "le premier ajout doit poser exactement une station : {apres_un:?}"
    );

    // Le testeur retire la station de ses favoris…
    let id = apres_un[0].0;
    RadioRepo::with_backend(state.backend.clone())
        .set_favorite(id, false)
        .expect("dé-favoriser doit marcher");

    // …puis clique sur le bouton de l'entrée HOMONYME, qui sert la même adresse
    // sous un autre nom. C'est le cas mesuré sur l'annuaire du 09/09.
    let (status, page) = ajouter_depuis_le_web(&app, "Exemple paradise - Main Mix", FLUX).await;
    assert_eq!(status, StatusCode::OK, "le second clic ne doit pas échouer");

    let apres_deux = stations_sur(&state, FLUX);
    assert_eq!(
        apres_deux.len(),
        1,
        "deux entrées d'annuaire sur le MÊME flux ont posé {} stations : \
         c'est le doublon que le testeur venait signaler — {apres_deux:?}",
        apres_deux.len()
    );
    assert_eq!(
        apres_deux[0].0, id,
        "la station d'origine doit être conservée, pas remplacée"
    );
    assert!(
        apres_deux[0].2,
        "le geste demandait d'avoir la station dans ses favoris : elle doit y \
         être remise — {apres_deux:?}"
    );
    // Et l'utilisateur doit l'APPRENDRE : une page qui dit « ajoutée » alors
    // que rien n'a été ajouté est un mensonge à l'écran.
    assert!(
        page.contains("déjà")
            || page.contains("already")
            || page.contains("bereits")
            || page.contains("已"),
        "la page ne dit pas que la station était déjà là : {page}"
    );
}

#[tokio::test]
async fn deux_flux_differents_du_meme_diffuseur_restent_deux_stations() {
    let (app, state) = app_et_etat();
    const AUTRE: &str = "http://stream.exemple-radio.test/rock-flacm";

    let (status, _) = ajouter_depuis_le_web(&app, "Exemple - Main Mix", FLUX).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = ajouter_depuis_le_web(&app, "Exemple - Rock Mix", AUTRE).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        stations_sur(&state, FLUX).len(),
        1,
        "le premier canal a disparu"
    );
    assert_eq!(
        stations_sur(&state, AUTRE).len(),
        1,
        "un SECOND canal du même diffuseur est une station à part entière : \
         la porte anti-doublon ne doit pas l'avaler"
    );
}

#[tokio::test]
async fn la_meme_adresse_ecrite_autrement_est_reconnue() {
    let (app, state) = app_et_etat();

    let (status, _) = ajouter_depuis_le_web(&app, "Exemple - Main Mix", FLUX).await;
    assert_eq!(status, StatusCode::OK);

    // Casse et `/` final : les trois écarts que `flux_normalise` ramène à une
    // seule écriture. Sans eux, l'annuaire poserait un doublon dès qu'une
    // entrée est saisie en majuscules.
    let (status, _) = ajouter_depuis_le_web(
        &app,
        "Exemple autrement",
        "http://STREAM.Exemple-Radio.test/flacm/",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let toutes = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .into_iter()
        .filter(|s| s.url.to_ascii_lowercase().contains("exemple-radio.test"))
        .count();
    assert_eq!(
        toutes, 1,
        "la même adresse écrite autrement a posé une seconde station"
    );
}
