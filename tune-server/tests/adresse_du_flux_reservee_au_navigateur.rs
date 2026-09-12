//! L'adresse du flux interne ne se publie QUE pour une zone navigateur (#3164).
//!
//! ## Le fait
//!
//! `/stream/{id}` n'admet qu'UN consommateur : le canal mpsc de
//! `tune-core/src/http/streamer.rs` fait `break` sur la première connexion dès
//! qu'une seconde s'ouvre sur la même session. La coupure est propre — un
//! `EOF`, la sortie journalise `local_audio_stream_eof` — mais elle ARRÊTE la
//! lecture en cours.
//!
//! `build_zone_json` (`playback.rs`) le savait et posait la garde : seule une
//! zone dont `output_type` vaut `"browser"` reçoit `stream_url`, parce que là
//! l'onglet EST la sortie. C'est le correctif du défaut d'eric (#954) —
//! « je ferme l'onglet et le son revient ».
//!
//! Cinq autres surfaces publiaient la MÊME adresse sans la garde, avec le même
//! commentaire « for browser playback zones » au-dessus et aucune condition :
//!
//! | surface | route |
//! |---|---|
//! | `zones.rs` liste | `GET /zones` |
//! | `zones.rs` fiche | `GET /zones/{id}` |
//! | `playback.rs` `zone_status` | `GET /zones/{id}/status` |
//! | `playback.rs` `build_zone_json_with_result` | les ~20 routes de lecture |
//! | `radios.rs` `play_radio` | `POST /radios/{id}/play/{zone_id}` |
//!
//! Un onglet du client web ouvert sur une zone DLNA/Chromecast/AirPlay/locale
//! tenait donc l'adresse exacte du flux que le renderer est en train de
//! consommer. Le client web actuel ne l'ouvre pas (il teste `isBrowserZone`
//! avant), mais le SERVEUR la donnait — et c'est le serveur qui doit refuser.
//!
//! ## Ce que ce fichier cloue
//!
//! Les DEUX sens, sur les trois routes atteignables en HTTP :
//!
//! - une zone `browser` en lecture reçoit toujours `stream_url` — fermer la
//!   porte à tout le monde serait une régression silencieuse, l'onglet
//!   n'aurait plus rien à brancher sur son `<audio>` ;
//! - une zone `dlna` dans le MÊME état ne la reçoit pas.
//!
//! Les tests passent par le VRAI routeur (`tune_server::routes::router`) et la
//! vraie fonction de décision (`zones::zone_recoit_l_adresse_du_flux`) : rien
//! n'est recopié ici. Retirer la garde d'un seul site fait tomber ce fichier.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

/// L'identifiant de session que porte la lecture simulée.
///
/// Une chaîne reconnaissable : les assertions vérifient qu'elle apparaît DANS
/// l'adresse publiée, pas seulement que la clé existe.
const SESSION: &str = "sess-3164-b7d24e";

fn app() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(path).body(Body::empty()).unwrap()).await
}

/// Crée une zone du type demandé et rend son id.
async fn creer_zone(app: &axum::Router, nom: &str, output_type: &str) -> i64 {
    let corps = json!({
        "name": nom,
        "output_type": output_type,
        // Une zone navigateur n'a pas de périphérique par construction ; une
        // zone DLNA en a un, sinon `online` la déclare hors ligne et le cas
        // cesserait de ressembler à celui du terrain.
        "output_device_id": (output_type != "browser").then_some("uuid-renderer-3164"),
    });
    let (status, body) = envoyer(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/zones")
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "création de zone : {body}");
    body["id"].as_i64().expect("un id de zone")
}

/// Met la zone en lecture avec une session de flux ouverte.
///
/// C'est l'état exact qui rend l'adresse publiable : `now_playing.stream_id`
/// renseigné. On passe par le VRAI gestionnaire de lecture — le même que
/// l'orchestrateur appelle — pour que les routes lisent ce que la production
/// leur donnerait.
async fn mettre_en_lecture(state: &AppState, zone_id: i64) {
    state
        .playback
        .play(
            zone_id,
            NowPlaying {
                title: "Une piste".into(),
                duration_ms: 240_000,
                source: "local".into(),
                stream_id: Some(SESSION.into()),
                ..Default::default()
            },
        )
        .await;
}

/// Prépare deux zones en lecture : l'onglet, et un renderer réseau.
async fn deux_zones() -> (axum::Router, i64, i64) {
    let (app, state) = app();
    let navigateur = creer_zone(&app, "Onglet", "browser").await;
    let renderer = creer_zone(&app, "Salon DLNA", "dlna").await;
    mettre_en_lecture(&state, navigateur).await;
    mettre_en_lecture(&state, renderer).await;
    (app, navigateur, renderer)
}

/// L'adresse publiée doit désigner la session ouverte, pas n'importe quoi.
fn exiger_l_adresse(charge: &Value, route: &str) {
    let url = charge["stream_url"].as_str().unwrap_or_else(|| {
        panic!(
            "{route} : une zone NAVIGATEUR en lecture doit recevoir `stream_url` \
             — sans elle le client web n'a rien à brancher sur son `<audio>` et \
             la zone reste muette. Charge utile : {charge}"
        )
    });
    assert!(
        url.contains(SESSION),
        "{route} : `stream_url` = {url:?} ne désigne pas la session ouverte \
         ({SESSION})"
    );
}

/// Et l'absence doit être TOTALE : ni `stream_url`, ni son jumeau distant.
fn exiger_le_silence(charge: &Value, route: &str) {
    assert!(
        charge.get("stream_url").is_none_or(Value::is_null),
        "{route} : une zone DLNA a reçu `stream_url` = {} — un onglet du client \
         web tient là de quoi ouvrir une SECONDE connexion sur la session que \
         le renderer consomme. `/stream/{{id}}` n'admet qu'un consommateur : la \
         première connexion casse sur un `EOF` et la lecture s'arrête (#3164, \
         défaut d'eric #954). Charge utile : {charge}",
        charge["stream_url"]
    );
    assert!(
        charge.get("stream_url_remote").is_none_or(Value::is_null),
        "{route} : `stream_url_remote` publiée pour une zone DLNA — l'adresse \
         de relais ouvre la même session que l'adresse locale, et coupe la \
         lecture de la même façon. Charge utile : {charge}"
    );
}

/// Retrouve une zone par son id dans la charge utile de `GET /zones`.
fn zone_dans_la_liste(liste: &Value, zone_id: i64) -> Value {
    liste
        .as_array()
        .expect("GET /zones rend un tableau")
        .iter()
        .find(|z| z["id"].as_i64() == Some(zone_id))
        .unwrap_or_else(|| panic!("zone {zone_id} absente de GET /zones : {liste}"))
        .clone()
}

#[tokio::test]
async fn liste_des_zones_reserve_l_adresse_au_navigateur() {
    let (app, navigateur, renderer) = deux_zones().await;
    let (status, liste) = get(&app, "/api/v1/zones").await;
    assert_eq!(status, StatusCode::OK, "{liste}");

    exiger_l_adresse(&zone_dans_la_liste(&liste, navigateur), "GET /zones");
    exiger_le_silence(&zone_dans_la_liste(&liste, renderer), "GET /zones");
}

#[tokio::test]
async fn fiche_de_zone_reserve_l_adresse_au_navigateur() {
    let (app, navigateur, renderer) = deux_zones().await;

    let (status, fiche) = get(&app, &format!("/api/v1/zones/{navigateur}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_l_adresse(&fiche, "GET /zones/{id}");

    let (status, fiche) = get(&app, &format!("/api/v1/zones/{renderer}")).await;
    assert_eq!(status, StatusCode::OK, "{fiche}");
    exiger_le_silence(&fiche, "GET /zones/{id}");
}

#[tokio::test]
async fn statut_de_zone_reserve_l_adresse_au_navigateur() {
    let (app, navigateur, renderer) = deux_zones().await;

    let (status, etat) = get(&app, &format!("/api/v1/zones/{navigateur}/status")).await;
    assert_eq!(status, StatusCode::OK, "{etat}");
    exiger_l_adresse(&etat, "GET /zones/{id}/status");

    let (status, etat) = get(&app, &format!("/api/v1/zones/{renderer}/status")).await;
    assert_eq!(status, StatusCode::OK, "{etat}");
    exiger_le_silence(&etat, "GET /zones/{id}/status");
}

/// La règle elle-même, appelée directement — pas une transcription.
///
/// Les trois tests ci-dessus prouvent que les routes s'y branchent ; celui-ci
/// dit ce qu'elle décide, y compris pour les types qui n'ont pas de route
/// dédiée dans ce fichier.
#[test]
fn la_regle_ne_laisse_passer_que_le_navigateur() {
    use tune_server::routes::zones::zone_recoit_l_adresse_du_flux as recoit;

    assert!(
        recoit(Some("browser")),
        "la zone navigateur DOIT recevoir l'adresse : l'onglet est la sortie"
    );
    for refuse in [
        "local",
        "dlna",
        "openhome",
        "chromecast",
        "bluos",
        "airplay",
        "slimproto",
        "snapcast",
    ] {
        assert!(
            !recoit(Some(refuse)),
            "une zone `{refuse}` ne doit PAS recevoir l'adresse : sa sortie \
             consomme déjà l'unique flux de la session"
        );
    }
    assert!(
        !recoit(None),
        "une zone sans `output_type` connu ne reçoit rien : le doute se \
         tranche du côté qui ne coupe pas la lecture"
    );
}

/// Verrou de branchement pour les deux surfaces que ce fichier n'atteint pas
/// en HTTP sans une lecture réelle.
///
/// `build_zone_json_with_result` n'est appelée que sur une lecture que
/// l'orchestrateur a menée jusqu'au bout (fichier réel, sortie jointe), et
/// `play_radio` sur une station qui répond. Les monter ici rendrait ce
/// fichier dépendant du réseau et du matériel — et un test lent finit par être
/// désarmé.
///
/// Ce contrôle exige donc l'autre preuve : que ces deux surfaces NOMMENT la
/// règle. Il ne dit pas qu'elles l'appliquent bien ; il dit qu'on ne peut pas
/// la leur retirer en silence. Le minimum est explicite — un garde-fou qui ne
/// trouve rien doit ÉCHOUER, pas passer au vert.
#[test]
fn les_surfaces_hors_http_nomment_la_regle() {
    const PLAYBACK: &str = include_str!("../src/routes/playback.rs");
    const RADIOS: &str = include_str!("../src/routes/radios.rs");
    const REGLE: &str = "zone_recoit_l_adresse_du_flux(";

    // `build_zone_json_with_result` écrase la décision de `build_zone_json`
    // avec `PlayResult::stream_url`, que l'orchestrateur remplit pour TOUTES
    // les zones. Sans la règle ici, la garde de `build_zone_json` ne vaut plus
    // rien sur les vingt routes de lecture qui passent par cette fonction.
    let corps = {
        const DEBUT: &str = "async fn build_zone_json_with_result(";
        let debut = PLAYBACK.find(DEBUT).unwrap_or_else(|| {
            panic!("`build_zone_json_with_result` renommée : la découpe ne garde plus rien")
        });
        let fin = PLAYBACK[debut..]
            .find("\n#[derive(Deserialize, Default)]")
            .map(|i| debut + i)
            .unwrap_or_else(|| panic!("fin de `build_zone_json_with_result` introuvable"));
        &PLAYBACK[debut..fin]
    };
    assert!(
        corps.contains(REGLE),
        "`build_zone_json_with_result` ne nomme plus `{REGLE}` : elle rend de \
         nouveau `PlayResult::stream_url` à toutes les zones, et annule la \
         garde de `build_zone_json` sur `play` / `next` / `previous` / \
         `resume` / `queue/jump` / `pins/{{i}}/invoke` (#3164). Corps lu :\n{corps}"
    );

    let dans_radios = RADIOS.matches(REGLE).count();
    assert!(
        dans_radios >= 1,
        "`radios.rs` ne nomme plus `{REGLE}` ({dans_radios} occurrence(s)) : \
         `POST /radios/{{id}}/play/{{zone_id}}` republierait l'adresse du \
         flux à une zone DLNA (#3164)."
    );
}
