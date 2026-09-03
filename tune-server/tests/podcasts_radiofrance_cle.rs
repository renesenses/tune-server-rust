//! Contrat des routes Radio France (GraphQL) quand la clé API manque (#1026).
//!
//! Le défaut d'origine : ouvrir « Podcasts » déclenche la sonde
//! `GET /api/v1/podcasts/radiofrance/shows`, et sans clé le serveur répondait
//! `400 {"code":"bad_request","error":"radiofrance_api_key not configured"}` —
//! une requête parfaitement formée passait pour fautive, et le seul texte
//! disponible était un message technique anglais citant un nom de variable.
//!
//! Le contrat tenu ici :
//!
//! 1. sans clé, les trois routes répondent `412 Precondition Failed` — un état
//!    de configuration, pas une erreur de requête — avec un code stable
//!    (`radiofrance_cle_absente`), le nom exact du réglage à renseigner
//!    (`radiofrance_api_key`) et un message dans la langue de l'interface ;
//! 2. une clé vide compte comme absente ;
//! 3. avec une clé, la porte s'ouvre : la même requête ne rend plus jamais
//!    `radiofrance_cle_absente` ;
//! 4. **le réglage que ce 412 désigne est réellement saisissable.** C'était la
//!    moitié manquante : le serveur renvoyait vers `radiofrance_api_key`, mais
//!    aucun écran n'offrait ce champ. Radio France n'apparaissait pas dans
//!    « Services & jetons » (`GET /api/v1/services/tokens`), la seule surface
//!    de saisie de clés du produit — le renvoi ne menait nulle part.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn lire(app: &axum::Router, chemin: &str, langue: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(chemin);
    if let Some(l) = langue {
        req = req.header("Accept-Language", l);
    }
    let reponse = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, corps)
}

const ROUTES_SOUS_CLE: [&str; 3] = [
    // La sonde exacte du crawler (#1026), envoyée à l'ouverture de l'écran.
    "/api/v1/podcasts/radiofrance/shows?station=FRANCEINTER",
    "/api/v1/podcasts/radiofrance/shows/search?q=histoire",
    "/api/v1/podcasts/radiofrance/episodes?show_url=https://example.net/emission",
];

// ---------------------------------------------------------------------------
// 1. Sans clé : un état de configuration exploitable, pas une requête fautive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sans_cle_les_routes_annoncent_configuration_requise() {
    let (app, _state) = app_et_etat();

    for chemin in ROUTES_SOUS_CLE {
        let (status, corps) = lire(&app, chemin, None).await;

        // 412, pas 400 : la requête est bonne, c'est le serveur qui n'est pas
        // prêt. Même statut que Discogs et setlist.fm pour leurs clés.
        assert_eq!(
            status,
            StatusCode::PRECONDITION_FAILED,
            "chemin = {chemin}, corps = {corps}"
        );

        // Le code stable, pour qui programme contre l'API.
        assert_eq!(
            corps["error"], "radiofrance_cle_absente",
            "chemin = {chemin}, corps = {corps}"
        );

        // Le réglage qui active la source, nommé tel quel : c'est ce qui
        // permet au client d'afficher « configuration requise » avec un
        // renvoi vers le bon champ, au lieu d'un message opaque.
        assert_eq!(
            corps["setting"], "radiofrance_api_key",
            "chemin = {chemin}, corps = {corps}"
        );

        // Et un message humain, français par défaut, qui nomme le réglage.
        let message = corps["message"].as_str().expect("message absent");
        assert!(
            message.contains("radiofrance_api_key"),
            "message = {message}"
        );
        assert!(message.contains("clé"), "message = {message}");
    }
}

#[tokio::test]
async fn sans_cle_le_message_suit_la_langue_de_l_interface() {
    let (app, _state) = app_et_etat();

    let (status, corps) = lire(
        &app,
        "/api/v1/podcasts/radiofrance/shows?station=FRANCEINTER",
        Some("en-GB,en;q=0.9"),
    )
    .await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "corps = {corps}");
    let message = corps["message"].as_str().expect("message absent");
    assert!(message.contains("API key"), "message = {message}");
    assert!(
        message.contains("radiofrance_api_key"),
        "message = {message}"
    );
}

#[tokio::test]
async fn une_cle_vide_compte_comme_absente() {
    let (app, state) = app_et_etat();
    SettingsRepo::with_backend(state.backend.clone())
        .set("radiofrance_api_key", "")
        .unwrap();

    let (status, corps) = lire(
        &app,
        "/api/v1/podcasts/radiofrance/shows?station=FRANCEINTER",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "corps = {corps}");
    assert_eq!(corps["error"], "radiofrance_cle_absente");
}

// ---------------------------------------------------------------------------
// 2. Avec une clé : la porte s'ouvre
// ---------------------------------------------------------------------------

/// Le témoin « avec clé » ne doit dépendre d'aucun réseau : on interroge une
/// station inconnue, refusée APRÈS la porte de la clé et AVANT tout appel à
/// l'API Radio France. Si la réponse est le 400 « unknown station » et non le
/// 412 « clé absente », c'est que la clé a bien été lue et acceptée.
#[tokio::test]
async fn avec_cle_la_porte_s_ouvre_sans_annoncer_configuration_requise() {
    let (app, state) = app_et_etat();
    SettingsRepo::with_backend(state.backend.clone())
        .set("radiofrance_api_key", "clef-de-test")
        .unwrap();

    let (status, corps) = lire(
        &app,
        "/api/v1/podcasts/radiofrance/shows?station=INEXISTANTE",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "corps = {corps}");
    assert_ne!(corps["error"], "radiofrance_cle_absente", "corps = {corps}");
    assert_eq!(corps["code"], "bad_request", "corps = {corps}");
}

// ---------------------------------------------------------------------------
// 3. Le réglage que le 412 désigne est saisissable — sinon le renvoi ment
// ---------------------------------------------------------------------------

const SERVICES: &str = "/api/v1/services/tokens";

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let recu: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, recu)
}

fn service<'a>(liste: &'a Value, id: &str) -> Option<&'a Value> {
    liste.as_array()?.iter().find(|s| s["id"] == id)
}

/// Le défaut restant de #1026 : le 412 nomme `radiofrance_api_key`, mais
/// « Services & jetons » — la seule surface où l'on saisit une clé — ignorait
/// Radio France. L'utilisateur lisait « renseignez le réglage
/// radiofrance_api_key » sans trouver nulle part où le renseigner.
#[tokio::test]
async fn radio_france_figure_dans_les_services_avec_son_champ_de_cle() {
    let (app, _state) = app_et_etat();

    let (status, liste) = lire(&app, SERVICES, None).await;
    assert_eq!(status, StatusCode::OK, "liste = {liste}");

    let rf = service(&liste, "radiofrance")
        .unwrap_or_else(|| panic!("aucun service « radiofrance » dans {liste}"));

    // Le champ de saisie, et sa clé : `save()` écrit `{id}_{champ}`, donc
    // « radiofrance » + « api_key » = `radiofrance_api_key`, exactement le
    // réglage que nomme le 412 et que lit la route Podcasts.
    let champs = rf["fields"].as_array().expect("fields absent");
    assert_eq!(champs.len(), 1, "rf = {rf}");
    assert_eq!(champs[0]["key"], "api_key", "rf = {rf}");
}

/// Le badge « configuré » se lit là où la route Podcasts lit la clé : il ne
/// peut donc pas annoncer une source active que l'écran Podcasts refuse.
/// Une clé vide compte comme absente des deux côtés.
#[tokio::test]
async fn le_badge_configure_dit_la_meme_chose_que_l_ecran_podcasts() {
    let (app, state) = app_et_etat();
    let reglages = SettingsRepo::with_backend(state.backend.clone());

    let (_, liste) = lire(&app, SERVICES, None).await;
    assert_eq!(
        service(&liste, "radiofrance").expect("service absent")["configured"],
        json!(false),
        "liste = {liste}"
    );

    // Clé vide : la route Podcasts la tient pour absente, le badge aussi.
    reglages.set("radiofrance_api_key", "").unwrap();
    let (_, liste) = lire(&app, SERVICES, None).await;
    assert_eq!(
        service(&liste, "radiofrance").expect("service absent")["configured"],
        json!(false),
        "liste = {liste}"
    );

    reglages.set("radiofrance_api_key", "clef-de-test").unwrap();
    let (_, liste) = lire(&app, SERVICES, None).await;
    assert_eq!(
        service(&liste, "radiofrance").expect("service absent")["configured"],
        json!(true),
        "liste = {liste}"
    );
}

/// La boucle complète, telle que la vit l'utilisateur : il lit le 412, ouvre
/// « Services & jetons », saisit sa clé dans le champ que le serveur annonce,
/// et la source s'ouvre.
#[tokio::test]
async fn saisir_la_cle_dans_les_services_ouvre_la_source_podcasts() {
    let (app, _state) = app_et_etat();

    let (status, corps) = lire(&app, ROUTES_SOUS_CLE[0], None).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "corps = {corps}");
    let reglage = corps["setting"]
        .as_str()
        .expect("setting absent")
        .to_string();

    // Le champ annoncé par la liste, pas un chemin deviné.
    let (_, liste) = lire(&app, SERVICES, None).await;
    let rf = service(&liste, "radiofrance").expect("service absent");
    let champ = rf["fields"][0]["key"].as_str().expect("champ absent");
    assert_eq!(
        format!("radiofrance_{champ}"),
        reglage,
        "le champ offert n'écrit pas le réglage que le 412 désigne"
    );

    let (status, _) = poster(
        &app,
        &format!("{SERVICES}/radiofrance"),
        json!({ champ: "clef-de-test" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Comme le témoin « avec clé » plus haut : station inconnue, refusée APRÈS
    // la porte de la clé et AVANT tout appel réseau à Radio France.
    let (status, corps) = lire(
        &app,
        "/api/v1/podcasts/radiofrance/shows?station=INEXISTANTE",
        None,
    )
    .await;
    assert_ne!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "la source refuse encore la clé saisie depuis « Services & jetons » : corps = {corps}"
    );
    assert_ne!(corps["error"], "radiofrance_cle_absente", "corps = {corps}");
}

/// Témoin anti-régression : ajouter Radio France n'a fait perdre aucun des
/// services déjà offerts. Vert des deux côtés du correctif.
#[tokio::test]
async fn les_services_deja_offerts_sont_tous_encore_la() {
    let (app, _state) = app_et_etat();
    let (status, liste) = lire(&app, SERVICES, None).await;
    assert_eq!(status, StatusCode::OK);

    for id in [
        "musicbrainz",
        "discogs",
        "lastfm",
        "listenbrainz",
        "genius",
        "tidal",
        "qobuz",
        "spotify",
        "deezer",
    ] {
        assert!(
            service(&liste, id).is_some(),
            "service « {id} » perdu : liste = {liste}"
        );
    }
}
