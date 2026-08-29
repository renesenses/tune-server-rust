//! `GET /radios/search` : « aucune station de ce nom » n'est plus la même
//! réponse que « la recherche a échoué » (#2119).
//!
//! Le défaut d'origine tient en une ligne, `routes/radios.rs` :
//!
//! ```ignore
//! let items = repo.search(&q.q).unwrap_or_default();
//! Json(json!(items))
//! ```
//!
//! Le `unwrap_or_default()` transformait toute erreur du dépôt en `[]` — le
//! corps EXACT que rend un catalogue qui ne connaît pas la station. Un client
//! ne pouvait donc pas écrire la bonne phrase, faute de savoir laquelle des
//! deux s'était produite.
//!
//! Ce n'est pas une inquiétude de principe : le 21/08/2026 (fil forum 1506),
//! Belkadi Yacine cherche « radio paradise », voit une liste vide et ouvre un
//! ticket « radio paradise ne fonctionne pas » ; Bilou, qui a la station dans
//! SON catalogue pour l'y avoir ajoutée, répond « fonctionne parfaitement chez
//! moi ». Deux verdicts opposés le même jour, aucun des deux faux.
//!
//! Les essais tiennent quatre propriétés, dans cet ordre :
//!
//! 1. une station présente se trouve, et le corps le DIT (pas seulement par la
//!    longueur de `items`) ;
//! 2. la requête du ticket rend « aucun résultat », avec le geste de secours à
//!    l'écran — c'est la voie 3 de l'issue, « au minimum, le dire » ;
//! 3. une recherche qui n'aboutit pas rend une PANNE, et non un catalogue
//!    vide ;
//! 4. **la contre-épreuve** : les deux issues précédentes ne partagent NI le
//!    statut HTTP, NI le code, NI le message. Sans cette assertion-là, rien ne
//!    prouve que la distinction est observable.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn chercher(app: &axum::Router, requete: &str) -> (StatusCode, Value) {
    chercher_dans_la_langue(app, requete, "fr-FR,fr;q=0.9").await
}

async fn chercher_dans_la_langue(
    app: &axum::Router,
    requete: &str,
    accept_language: &str,
) -> (StatusCode, Value) {
    let chemin = format!("/api/v1/radios/search?q={}", urlencoding_minimal(requete));
    let reponse = app
        .clone()
        .oneshot(
            Request::get(&chemin)
                .header("Accept-Language", accept_language)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, corps)
}

/// Assez pour les requêtes de ces essais : seul l'espace a besoin d'être
/// encodé. Une dépendance de plus pour trois caractères ne se justifierait pas.
fn urlencoding_minimal(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20")
}

fn semer_station(state: &tune_server::state::AppState, nom: &str, url: &str) -> i64 {
    RadioRepo::with_backend(state.backend.clone())
        .create(&RadioStation {
            id: None,
            name: nom.into(),
            url: url.into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .expect("l'insertion directe en base doit réussir")
}

/// Met le dépôt hors d'état de répondre.
///
/// C'est la seule façon d'atteindre la branche d'erreur sans mentir sur le
/// chemin : `repo.search` rend un `Err` parce que la table n'existe plus,
/// exactement comme il le ferait sur un schéma incomplet ou une base fermée.
fn casser_le_catalogue(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch("DROP TABLE radio_stations")
        .expect("le catalogue doit pouvoir être retiré pour l'essai");
}

// ---------------------------------------------------------------------------
// 1. Une station présente se trouve — et le corps le dit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_station_du_catalogue_se_trouve_et_le_statut_le_dit() {
    let (app, state) = app_et_etat();
    semer_station(
        &state,
        "Radio Paradise Rock Mix",
        "http://stream.radioparadise.com/rock-flacm",
    );

    let (status, corps) = chercher(&app, "paradise").await;

    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "resultats", "corps = {corps}");
    assert_eq!(
        corps["code"], "radio_recherche_resultats",
        "corps = {corps}"
    );
    assert_eq!(corps["count"], 1, "corps = {corps}");
    assert_eq!(
        corps["items"].as_array().map(Vec::len),
        Some(1),
        "corps = {corps}"
    );
    assert_eq!(
        corps["items"][0]["name"], "Radio Paradise Rock Mix",
        "corps = {corps}"
    );
    // Rien à dire quand la liste parle d'elle-même : un message ici ferait
    // écrire au client une phrase par-dessus des résultats.
    assert!(corps["message"].is_null(), "corps = {corps}");
}

// ---------------------------------------------------------------------------
// 2. La requête du ticket : « aucun résultat », dit comme tel
// ---------------------------------------------------------------------------

/// Reproduit la mesure du ticket sur un serveur neuf.
///
/// Si le catalogue livré gagne un jour Radio Paradise (voie 1 de l'issue,
/// encore à trancher), cet essai deviendra rouge : c'est voulu. Il faudra
/// alors changer la requête, pas l'assertion — la propriété tenue ici est
/// « une station absente rend `aucun_resultat` », pas « Radio Paradise est
/// absente ».
#[tokio::test]
async fn la_requete_du_ticket_rend_aucun_resultat_et_le_geste_de_secours() {
    let (app, _state) = app_et_etat();

    let (status, corps) = chercher(&app, "radio paradise").await;

    // Une recherche qui aboutit sur zéro station a RÉUSSI.
    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "aucun_resultat", "corps = {corps}");
    assert_eq!(
        corps["code"], "radio_recherche_aucun_resultat",
        "corps = {corps}"
    );
    assert_eq!(corps["count"], 0, "corps = {corps}");
    assert_eq!(corps["items"], json!([]), "corps = {corps}");

    // Voie 3 de l'issue : « au minimum, le dire ». Le message doit nommer le
    // catalogue ET le geste de secours, sinon il ne fait pas gagner la minute
    // que Yacine a perdue.
    let message = corps["message"].as_str().expect("message absent");
    assert!(message.contains("catalogue Tune"), "message = {message}");
    assert!(message.contains("adresse"), "message = {message}");

    // Et la réponse qualifie sa portée : « absente de CE catalogue » n'est pas
    // « inexistante ».
    assert_eq!(corps["portee"], "catalogue_local", "corps = {corps}");
}

// ---------------------------------------------------------------------------
// 3. Une recherche qui n'aboutit pas est une panne, pas un catalogue vide
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_recherche_qui_echoue_ne_se_lit_plus_comme_un_catalogue_vide() {
    let (app, state) = app_et_etat();
    casser_le_catalogue(&state);

    let (status, corps) = chercher(&app, "paradise").await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "une panne doit se lire au code de retour, corps = {corps}"
    );
    assert_eq!(corps["statut"], "echec", "corps = {corps}");
    assert_eq!(corps["code"], "radio_recherche_echec", "corps = {corps}");

    // La forme ne change pas : `items` est là, vide, pour qu'un client n'ait
    // pas à deviner la structure avant de savoir ce qui s'est passé.
    assert_eq!(corps["items"], json!([]), "corps = {corps}");
    assert_eq!(corps["count"], 0, "corps = {corps}");

    // Le message dit que ce n'est PAS une station manquante — c'est toute la
    // confusion du fil 1506.
    let message = corps["message"].as_str().expect("message absent");
    assert!(
        message.contains("n'est pas une station manquante")
            || message.contains("pas une station manquante"),
        "message = {message}"
    );

    // La cause technique reste disponible pour le rapport de bogue.
    let detail = corps["detail"].as_str().expect("detail absent");
    assert!(!detail.trim().is_empty(), "detail = {detail}");
}

// ---------------------------------------------------------------------------
// 4. CONTRE-ÉPREUVE — les deux issues sont bien discernables
// ---------------------------------------------------------------------------

/// L'assertion qui donne son sens aux trois précédentes.
///
/// Chacune prise seule pourrait passer sur une implémentation qui rendrait le
/// même corps dans les deux cas. Ici, on met les deux réponses côte à côte et
/// on exige qu'elles diffèrent sur les TROIS canaux qu'un client peut lire :
/// le statut HTTP, le code stable, le message montré.
#[tokio::test]
async fn aucun_resultat_et_panne_different_sur_les_trois_canaux_lisibles() {
    let (app_sain, _etat_sain) = app_et_etat();
    let (app_casse, etat_casse) = app_et_etat();
    casser_le_catalogue(&etat_casse);

    let (statut_aucun, corps_aucun) = chercher(&app_sain, "paradise").await;
    let (statut_panne, corps_panne) = chercher(&app_casse, "paradise").await;

    assert_ne!(
        statut_aucun, statut_panne,
        "statut identique : {statut_aucun} pour les deux"
    );
    assert_ne!(
        corps_aucun["code"], corps_panne["code"],
        "code identique : {}",
        corps_aucun["code"]
    );
    assert_ne!(
        corps_aucun["message"], corps_panne["message"],
        "message identique : {}",
        corps_aucun["message"]
    );

    // Et la régression exacte d'avant ce correctif : les deux corps ne peuvent
    // plus être le même tableau vide.
    assert_ne!(corps_aucun, json!([]), "corps = {corps_aucun}");
    assert_ne!(corps_panne, json!([]), "corps = {corps_panne}");
    assert_ne!(corps_aucun, corps_panne);
}

// ---------------------------------------------------------------------------
// 5. Le message suit la langue de l'interface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_message_daucun_resultat_suit_la_langue_demandee() {
    let (app, _state) = app_et_etat();

    let (_, en_fr) = chercher_dans_la_langue(&app, "paradise", "fr-FR,fr;q=0.9").await;
    let (_, en_en) = chercher_dans_la_langue(&app, "paradise", "en-GB,en;q=0.9").await;

    let fr = en_fr["message"].as_str().expect("message fr absent");
    let en = en_en["message"].as_str().expect("message en absent");
    assert!(fr.contains("catalogue Tune"), "fr = {fr}");
    assert!(en.contains("Tune catalogue"), "en = {en}");
    assert_ne!(fr, en, "la traduction n'a pas été appliquée");

    // Le code, lui, ne bouge pas d'une langue à l'autre : c'est ce contre quoi
    // un client programme.
    assert_eq!(en_fr["code"], en_en["code"]);
}

// ---------------------------------------------------------------------------
// 6. Le catalogue livré reste le seul interrogé — et la réponse l'annonce
// ---------------------------------------------------------------------------

/// La portée est un relevé, pas une promesse.
///
/// Brancher l'annuaire public de mozaiklabs.fr sur la recherche est la voie 1
/// de l'issue, laissée à l'arbitrage. Tant qu'elle n'est pas tranchée, la
/// réponse doit dire honnêtement où elle a cherché — sans quoi « pas trouvé »
/// continue de se lire « n'existe pas ».
#[tokio::test]
async fn la_portee_est_annoncee_sur_les_trois_issues() {
    let (app_sain, etat_sain) = app_et_etat();
    semer_station(
        &etat_sain,
        "FIP Rock",
        "https://icecast.radiofrance.fr/fiprock.mp3",
    );
    let (app_casse, etat_casse) = app_et_etat();
    casser_le_catalogue(&etat_casse);

    for (nom, corps) in [
        ("resultats", chercher(&app_sain, "fip rock").await.1),
        ("aucun", chercher(&app_sain, "paradise").await.1),
        ("panne", chercher(&app_casse, "paradise").await.1),
    ] {
        assert_eq!(
            corps["portee"], "catalogue_local",
            "portée absente pour {nom} : {corps}"
        );
    }
}
