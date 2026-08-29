//! Contrat de la validation d'adresse de flux radio, vue depuis l'API (#2097).
//!
//! Le défaut d'origine : Tune acceptait `http;//…`, le stockait, le proposait
//! à la lecture, et ne produisait que du silence — Tades a dû relire l'adresse
//! caractère par caractère pour trouver le point-virgule (fil forum 1484).
//!
//! Ces essais tiennent les trois propriétés qui comptent, dans cet ordre :
//!
//! 1. la route REFUSE l'adresse du ticket, avec un message qui dit quoi
//!    corriger — c'est le serveur qui refuse, pas le navigateur, parce que
//!    l'API est appelable directement ;
//! 2. elle ACCEPTE les adresses biscornues mais lisibles — une station qui
//!    marchait doit rester enregistrable, sans quoi le correctif serait pire
//!    que le défaut ;
//! 3. elle n'invalide RIEN de ce qui est déjà en base.

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

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, corps)
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::post(chemin)
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn poster_en_anglais(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::post(chemin)
            .header("Content-Type", "application/json")
            .header("Accept-Language", "en-GB,en;q=0.9")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn mettre_a_jour(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::put(chemin)
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(chemin).body(Body::empty()).unwrap()).await
}

/// Insère une station DIRECTEMENT en base, sans passer par la route — donc
/// sans validation. C'est ainsi qu'on reproduit une entrée créée avant ce
/// correctif.
fn semer_station_heritee(state: &tune_server::state::AppState, nom: &str, url: &str) -> i64 {
    let repo = RadioRepo::with_backend(state.backend.clone());
    repo.create(&RadioStation {
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

// ---------------------------------------------------------------------------
// 1. Le cas du ticket est refusé, et le refus est utile
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creer_une_radio_avec_le_point_virgule_du_ticket_est_refuse() {
    let (app, state) = app_et_etat();
    let avant = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .len();

    let (status, corps) = poster(
        &app,
        "/api/v1/radios",
        json!({"name": "ClassicHD", "stream_url": "http;//classic-hd.example.net/stream"}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "corps = {corps}");
    assert_eq!(corps["error"], "radio_url_separateur_faux");

    // Le message doit désigner la faute. « URL invalide » n'aurait rien
    // appris à Tades ; le nom du schéma attendu, si.
    let message = corps["message"].as_str().expect("message absent");
    assert!(message.contains("http://"), "message = {message}");
    assert!(message.contains("deux-points"), "message = {message}");
    assert!(message.contains("http;//"), "message = {message}");

    // Et rien n'a été écrit : la station muette n'existe pas.
    let apres = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap();
    assert_eq!(
        apres.len(),
        avant,
        "une station a été créée malgré le refus"
    );
    assert!(!apres.iter().any(|s| s.name == "ClassicHD"));
}

/// Le refus parle la langue de l'interface : le client envoie sa locale en
/// `Accept-Language`, et le message est composé côté serveur.
#[tokio::test]
async fn le_refus_est_traduit_selon_accept_language() {
    let (app, _state) = app_et_etat();
    let (status, corps) = poster_en_anglais(
        &app,
        "/api/v1/radios",
        json!({"name": "ClassicHD", "stream_url": "http;//classic-hd.example.net/stream"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = corps["message"].as_str().expect("message absent");
    assert!(message.contains("colon"), "message = {message}");
    assert!(
        !message.contains("deux-points"),
        "message resté en français : {message}"
    );
}

// ---------------------------------------------------------------------------
// 2. Le vrai risque : refuser une adresse qui marchait
// ---------------------------------------------------------------------------

/// Chacune de ces formes est lisible par le chemin de lecture réel (un GET
/// HTTP). Les refuser transformerait un défaut d'affichage en régression
/// bloquante pour des utilisateurs dont la radio fonctionnait.
#[tokio::test]
async fn les_adresses_exotiques_mais_lisibles_sont_acceptees() {
    let (app, state) = app_et_etat();
    let base = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .len();

    let legitimes = [
        (
            "port explicite",
            "http://icecast.example.net:8000/stream.mp3",
        ),
        ("chemin vide", "https://example.net"),
        ("IPv4 nue", "http://192.168.1.42:8000/"),
        ("IPv6 littérale", "http://[2001:db8::1]:8000/stream"),
        ("hôte sans point", "http://nas:8000/flux"),
        (
            "sous-domaines et paramètres",
            "https://stream.relay.eu-west.cdn.radio.example.net/live/aac?bitrate=320&session=abc",
        ),
        (
            "identifiants",
            "http://user:motdepasse@example.net:8000/stream",
        ),
        ("schéma en majuscules", "HTTP://EXAMPLE.NET/Stream.MP3"),
        ("playlist m3u", "http://example.net/live.m3u"),
        ("manifeste HLS", "https://example.net/hls/master.m3u8"),
    ];
    assert_eq!(legitimes.len(), 10, "le lot témoin a changé de taille");

    for (etiquette, url) in legitimes {
        let (status, corps) = poster(
            &app,
            "/api/v1/radios",
            json!({"name": format!("Station {etiquette}"), "stream_url": url}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{etiquette} ({url}) refusée à tort : {corps}"
        );
    }

    let apres = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap();
    assert_eq!(
        apres.len(),
        base + legitimes.len(),
        "toutes les stations légitimes doivent être en base"
    );
}

// ---------------------------------------------------------------------------
// 3. Rien de ce qui est déjà en base n'est invalidé
// ---------------------------------------------------------------------------

/// Une station enregistrée AVANT ce correctif, avec l'adresse même du ticket,
/// doit rester listée, lisible à l'unité, et MODIFIABLE sur ses autres champs.
///
/// C'est la propriété la plus facile à casser sans s'en apercevoir : il aurait
/// suffi de valider `station.url` après fusion, plutôt que `body.url` à la
/// saisie, pour qu'un utilisateur ne puisse plus renommer sa station sans
/// réparer d'abord une adresse qu'il n'a peut-être jamais tapée.
#[tokio::test]
async fn une_entree_deja_en_base_nest_pas_invalidee() {
    let (app, state) = app_et_etat();
    let heritee = "http;//classic-hd.example.net/stream";
    let id = semer_station_heritee(&state, "ClassicHD héritée", heritee);

    // Elle reste dans la liste.
    let (status, corps) = lire(&app, "/api/v1/radios").await;
    assert_eq!(status, StatusCode::OK);
    let listee = corps
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == json!(id))
        .expect("la station héritée a disparu de la liste");
    assert_eq!(listee["stream_url"], json!(heritee));

    // Elle reste lisible à l'unité, adresse intacte.
    let (status, corps) = lire(&app, &format!("/api/v1/radios/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["stream_url"], json!(heritee));

    // Et elle reste modifiable sur ses autres champs, SANS toucher l'adresse.
    let (status, corps) = mettre_a_jour(
        &app,
        &format!("/api/v1/radios/{id}"),
        json!({"name": "ClassicHD renommée", "genre": "Classique"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["name"], json!("ClassicHD renommée"));
    assert_eq!(
        corps["stream_url"],
        json!(heritee),
        "l'adresse héritée a été modifiée à l'insu de l'utilisateur"
    );

    // Vérification en base, pas seulement dans la réponse.
    let en_base = RadioRepo::with_backend(state.backend.clone())
        .get(id)
        .unwrap()
        .expect("la station héritée a été supprimée");
    assert_eq!(en_base.url, heritee);
    assert_eq!(en_base.name, "ClassicHD renommée");
}

/// L'autre moitié de la même propriété : dès que l'utilisateur RESAISIT
/// l'adresse, elle est validée — et une correction valide passe.
#[tokio::test]
async fn resaisir_ladresse_dune_entree_heritee_est_valide() {
    let (app, state) = app_et_etat();
    let id = semer_station_heritee(
        &state,
        "ClassicHD héritée",
        "http;//classic-hd.example.net/stream",
    );

    // Resaisie encore fautive : refusée, et la base ne bouge pas.
    let (status, corps) = mettre_a_jour(
        &app,
        &format!("/api/v1/radios/{id}"),
        json!({"stream_url": "http;//classic-hd.example.net/autre"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "corps = {corps}");
    assert_eq!(corps["error"], "radio_url_separateur_faux");
    assert_eq!(
        RadioRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .unwrap()
            .url,
        "http;//classic-hd.example.net/stream",
        "une adresse refusée ne doit pas être écrite"
    );

    // Resaisie corrigée : acceptée.
    let (status, corps) = mettre_a_jour(
        &app,
        &format!("/api/v1/radios/{id}"),
        json!({"stream_url": "http://classic-hd.example.net/stream"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(
        RadioRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .unwrap()
            .url,
        "http://classic-hd.example.net/stream"
    );
}

/// Les stations semées à la création d'une base neuve (migration
/// `seed_default_radios`) ne doivent évidemment pas être refusées par la
/// nouvelle règle. Si l'une d'elles l'était, le défaut serait dans le semis,
/// et il faut le savoir ici plutôt que chez un utilisateur.
#[tokio::test]
async fn les_stations_semees_par_defaut_respectent_la_regle() {
    let (_app, state) = app_et_etat();
    let semees = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap();
    assert!(
        !semees.is_empty(),
        "aucune station semée : le test ne prouverait rien"
    );
    let fautives: Vec<_> = semees
        .iter()
        .filter(|s| {
            let bas = s.url.to_ascii_lowercase();
            !(bas.starts_with("http://") || bas.starts_with("https://"))
        })
        .map(|s| format!("{} → {}", s.name, s.url))
        .collect();
    assert!(
        fautives.is_empty(),
        "{} station(s) semée(s) sur {} porteraient une adresse refusée : {fautives:?}",
        fautives.len(),
        semees.len()
    );
}

// ---------------------------------------------------------------------------
// 4. La page HTML du lien « ajouter à Tune » cite l'adresse — sans la jouer
// ---------------------------------------------------------------------------

/// `add_from_web` répond en HTML, et le message de refus CITE l'adresse
/// reçue. Sans échappement, `?url=<script>…` ferait de la page d'erreur un
/// vecteur d'injection : le correctif introduirait une faille en réparant un
/// silence.
#[tokio::test]
async fn la_page_dajout_depuis_le_web_nexecute_pas_ladresse_recue() {
    let (app, state) = app_et_etat();
    let avant = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .len();

    let reponse = app
        .clone()
        .oneshot(
            Request::get("/api/v1/radios/add?name=Piegee&url=%3Cscript%3Ealert(1)%3C%2Fscript%3E")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK, "la page doit être rendue");
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let page = String::from_utf8_lossy(&octets);

    assert!(
        !page.contains("<script>"),
        "balisage de l'adresse rendu tel quel : {page}"
    );
    assert!(
        page.contains("&lt;script&gt;"),
        "l'adresse fautive doit être citée, échappée : {page}"
    );
    assert_eq!(
        RadioRepo::with_backend(state.backend.clone())
            .list()
            .unwrap()
            .len(),
        avant,
        "aucune station ne doit être créée par cette adresse"
    );
}
