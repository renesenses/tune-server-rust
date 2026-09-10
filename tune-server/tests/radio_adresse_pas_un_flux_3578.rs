//! Une adresse de station qui rend une PAGE WEB est refusée à
//! l'ENREGISTREMENT, plus seulement à la lecture (#3578).
//!
//! Belkadi Yacine a saisi « Radio Paradise » en collant
//! `https://radioparadise.com/listen/channels/main-mix` — la page d'écoute, pas
//! le flux. Cette adresse passe toutes les règles de FORME de #2097 : `https`,
//! un hôte, aucun blanc. Tune l'a donc enregistrée sans un mot, et ne le lui a
//! appris qu'à la première lecture :
//!
//! ```text
//! radio_decode_to_wav_for_local_output url=https://radioparadise.com/listen/channels/main-mix
//! WARN radio_local_decode_failed error=radio_not_audio: le serveur a répondu « text/html »
//! ```
//!
//! Le juge — `tune_core::orchestrator::non_audio_content_type` — existait donc
//! déjà, et n'avait qu'UN appelant : la lecture. Ces essais tiennent le second,
//! `routes::radios::sonder_le_flux`, par le SITE D'APPEL : la route `POST
//! /api/v1/radios` et la route `PUT /api/v1/radios/{id}`, appelées par leur
//! vraie URL contre un serveur de station simulé.
//!
//! Les quatre essais qui suivent la garde sont sa CONTRE-ÉPREUVE : une sonde
//! qui refuserait tout passerait le premier essai et échouerait sur eux. Ce
//! qu'ils fixent, c'est que le refus ne porte QUE sur ce qui est établi —
//! serveur audio, serveur éteint, serveur qui ne sert pas `HEAD`, et modif qui
//! ne touche pas à l'adresse.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::RadioRepo;

// ---------------------------------------------------------------------------
// Le serveur de station simulé
// ---------------------------------------------------------------------------

/// Monte un faux serveur de station et rend son adresse de base.
///
/// Trois chemins, qui couvrent les trois comportements réels rencontrés :
///   * `/page` — 200 en `text/html`, le cas Radio Paradise ;
///   * `/flux` — 200 en `audio/mpeg`, une station ordinaire ;
///   * `/sans-head` — 405 sur `HEAD`, 200 en `audio/aacp` sur `GET`, ce que
///     font beaucoup d'Icecast.
async fn station_simulee() -> String {
    let app = axum::Router::new()
        .route(
            "/page",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=UTF-8")],
                    "<html><body>Écoutez-nous ici</body></html>",
                )
                    .into_response()
            }),
        )
        .route(
            "/flux",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/mpeg")],
                    "\u{ff}\u{fb}",
                )
                    .into_response()
            }),
        )
        .route(
            "/sans-head",
            any(|methode: axum::http::Method| async move {
                if methode == axum::http::Method::HEAD {
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/aacp")],
                    "\u{ff}\u{f1}",
                )
                    .into_response()
            }),
        );

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}")
}

/// Une adresse sur un port fermé : rien n'écoute, la sonde ne peut RIEN
/// établir. C'est le cas de l'Icecast de salon éteint au moment de la saisie.
async fn adresse_injoignable() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    drop(ecoute);
    format!("http://{adresse}/flux")
}

// ---------------------------------------------------------------------------
// Le banc
// ---------------------------------------------------------------------------

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, corps)
}

async fn creer(app: &axum::Router, nom: &str, url: &str) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::post("/api/v1/radios")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({"name": nom, "stream_url": url}).to_string(),
            ))
            .unwrap(),
    )
    .await
}

async fn modifier(app: &axum::Router, id: i64, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::put(format!("/api/v1/radios/{id}"))
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// Le nombre de stations en base.
///
/// JAMAIS comparé à un absolu : les migrations sèment l'annuaire livré (FIP,
/// France Musique, Radio Paradise…). Seul l'ÉCART avant/après a un sens.
fn nombre_de_stations(state: &tune_server::state::AppState) -> usize {
    RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .len()
}

// ---------------------------------------------------------------------------
// 1. La garde : le cas du ticket est refusé, et rien n'est écrit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_page_web_collee_dans_le_champ_du_flux_est_refusee_a_l_enregistrement() {
    let (app, state) = app_et_etat();
    let station = station_simulee().await;
    let avant = nombre_de_stations(&state);

    let (status, corps) = creer(&app, "Radio Paradise", &format!("{station}/page")).await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "une page web n'est pas un flux : corps = {corps}"
    );
    assert_eq!(
        corps["error"], "radio_url_pas_un_flux",
        "code stable attendu : {corps}"
    );
    let message = corps["message"].as_str().expect("message absent");
    assert!(
        message.contains("text/html"),
        "le message doit NOMMER ce que le serveur a rendu : {message}"
    );

    assert_eq!(
        nombre_de_stations(&state),
        avant,
        "rien ne doit être écrit : la station muette ne doit pas exister"
    );
}

#[tokio::test]
async fn modifier_une_station_vers_une_page_web_est_refuse() {
    let (app, state) = app_et_etat();
    let station = station_simulee().await;

    let (status, corps) = creer(&app, "Une station", &format!("{station}/flux")).await;
    assert_eq!(status, StatusCode::CREATED, "{corps}");
    let id = corps["id"]
        .as_i64()
        .expect("identifiant de la station créée");

    let (status, corps) =
        modifier(&app, id, json!({"stream_url": format!("{station}/page")})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "radio_url_pas_un_flux");

    // Et l'adresse d'origine est intacte.
    let station_en_base = RadioRepo::with_backend(state.backend.clone())
        .get(id)
        .unwrap()
        .expect("la station doit exister");
    assert_eq!(
        station_en_base.url,
        format!("{station}/flux"),
        "un refus ne doit rien écrire"
    );
}

// ---------------------------------------------------------------------------
// 2. Contre-épreuve : le refus ne porte QUE sur ce qui est établi
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_vraie_station_audio_est_enregistree() {
    let (app, state) = app_et_etat();
    let station = station_simulee().await;
    let avant = nombre_de_stations(&state);

    let (status, corps) = creer(&app, "Une station", &format!("{station}/flux")).await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "un serveur qui rend de l'audio doit passer : {corps}"
    );
    assert_eq!(nombre_de_stations(&state), avant + 1);
}

#[tokio::test]
async fn un_serveur_qui_ne_repond_pas_ne_fait_pas_refuser_la_station() {
    let (app, state) = app_et_etat();
    let injoignable = adresse_injoignable().await;
    let avant = nombre_de_stations(&state);

    let (status, corps) = creer(&app, "Icecast du salon", &injoignable).await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "un serveur éteint n'établit RIEN : la station reste enregistrable ({corps})"
    );
    assert_eq!(nombre_de_stations(&state), avant + 1);
}

#[tokio::test]
async fn un_serveur_qui_refuse_head_est_sonde_par_get() {
    let (app, state) = app_et_etat();
    let station = station_simulee().await;
    let avant = nombre_de_stations(&state);

    let (status, corps) = creer(&app, "Icecast", &format!("{station}/sans-head")).await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "HEAD refusé, GET audio : la station passe ({corps})"
    );
    assert_eq!(nombre_de_stations(&state), avant + 1);
}

#[tokio::test]
async fn renommer_une_station_ne_repasse_pas_son_adresse_par_la_sonde() {
    let (app, state) = app_et_etat();
    let station = station_simulee().await;

    // Une station héritée, dont l'adresse EST une page web : elle a été créée
    // avant #3578 et doit rester modifiable sans réparer son adresse d'abord.
    let id = RadioRepo::with_backend(state.backend.clone())
        .create(&tune_core::db::radio_repo::RadioStation {
            id: None,
            name: "Radio Paradise".into(),
            url: format!("{station}/page"),
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
        .expect("insertion directe en base");

    let (status, corps) = modifier(&app, id, json!({"name": "Radio Paradise (Main)"})).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "la modification ne porte pas sur l'adresse : rien à sonder ({corps})"
    );
    let station_en_base = RadioRepo::with_backend(state.backend.clone())
        .get(id)
        .unwrap()
        .expect("la station doit exister");
    assert_eq!(station_en_base.name, "Radio Paradise (Main)");
}

// ---------------------------------------------------------------------------
// 3. Le résidu #3664 : le refus PROPOSE la station du catalogue qui porte le
//    bon flux
// ---------------------------------------------------------------------------
//
// Le premier manque du ticket d'origine — l'adresse n'est jamais validée à
// l'enregistrement — est tenu par les essais ci-dessus. Le SECOND ne l'était
// pas : « le message de refus dit "cherchez le lien écouter sur le site de la
// radio", jamais "Tune connaît déjà Radio Paradise – Main Mix" » (#3664).
//
// Les deux rapprochements sont éprouvés séparément, puis leur contre-épreuve :
// un refus qui ne rapproche rien doit rendre une liste VIDE, pas la première
// station venue.

/// Les noms des stations proposées par un refus.
///
/// `suggestions` doit être un TABLEAU, toujours — même vide. Un `null` obligerait
/// l'écran à distinguer « aucune suggestion » de « ce serveur n'en propose
/// pas », exactement ce que `corps_recherche` refuse de lui imposer.
fn noms_proposes(corps: &Value) -> Vec<String> {
    corps["suggestions"]
        .as_array()
        .unwrap_or_else(|| panic!("`suggestions` absent ou pas un tableau : {corps}"))
        .iter()
        .map(|s| {
            s["name"]
                .as_str()
                .unwrap_or_else(|| panic!("suggestion sans nom : {s}"))
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn le_refus_propose_la_station_du_catalogue_qui_porte_le_meme_nom() {
    let (app, _state) = app_et_etat();
    let station = station_simulee().await;

    // Le nom saisi par Belkadi Yacine, et une page web à la place du flux.
    let (status, corps) = creer(&app, "Radio Paradise", &format!("{station}/page")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "radio_url_pas_un_flux");

    let noms = noms_proposes(&corps);
    assert!(
        noms.iter().any(|n| n == "Radio Paradise - Main Mix"),
        "le catalogue livré porte « Radio Paradise - Main Mix » depuis la \
         migration 90 : le refus doit la proposer, il propose {noms:?}"
    );
    // Le message d'origine n'est pas remplacé : il est toujours là, et il
    // nomme toujours ce que le serveur a rendu.
    assert!(
        corps["message"]
            .as_str()
            .is_some_and(|m| m.contains("text/html")),
        "le message de refus a été perdu : {corps}"
    );
}

#[tokio::test]
async fn le_refus_propose_la_station_du_catalogue_qui_partage_le_diffuseur() {
    let (app, _state) = app_et_etat();
    let station = station_simulee().await;

    // Une station du catalogue local sur CE diffuseur…
    let (status, corps) = creer(&app, "Le flux qui marche", &format!("{station}/flux")).await;
    assert_eq!(status, StatusCode::CREATED, "{corps}");

    // …et un refus dont le NOM ne ressemble à rien de connu. Seul l'hôte les
    // rapproche — c'est le cas du ticket, où la page d'écoute vit sur
    // `radioparadise.com` et le flux sur `stream.radioparadise.com`.
    let (status, corps) = creer(&app, "Zzzz", &format!("{station}/page")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    let noms = noms_proposes(&corps);
    assert!(
        noms.iter().any(|n| n == "Le flux qui marche"),
        "une station du catalogue vit sur le même diffuseur que l'adresse \
         refusée : le refus doit la proposer, il propose {noms:?}"
    );
}

#[tokio::test]
async fn un_refus_qui_ne_rapproche_rien_ne_propose_rien() {
    let (app, _state) = app_et_etat();

    // Adresse malformée (aucun hôte à comparer) et nom qu'aucune station du
    // catalogue livré ne porte : la liste doit être vide, et PRÉSENTE.
    let (status, corps) = creer(&app, "Zzyxwv", "http;//zzyxwv.example.net/flux").await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "radio_url_separateur_faux");
    assert_eq!(
        noms_proposes(&corps),
        Vec::<String>::new(),
        "rien ne rapproche cette saisie du catalogue : proposer quoi que ce \
         soit serait proposer au hasard"
    );
}
