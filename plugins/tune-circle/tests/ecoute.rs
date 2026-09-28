//! Tune Circle, étape T4 (#5327) : écouter chez un contact, côté AUDITEUR.
//!
//! Un seul faux serveur joue les trois rôles que le greffon appelle :
//! mozaiklabs (`POST /api/v1/circle/contacts/{id}/listen`, site-mozaiklabs#237),
//! le pont (`GET /stream/circle/{billet}`) et la route locale de lecture
//! (`POST /api/v1/zones/{id}/play`). Il note tout ce qu'il reçoit.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tune_circle::ecoute::{EVENEMENT_FIN_DE_PARTAGE, Ecoute};
use tune_circle::relais::Relais;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::event_bus::{EventBus, TuneEvent};

const JETON: &str = "jeton-acces-SECRET-5327";
const JETON_TUNE: &str = "Bearer jwt-de-l-auditeur";

#[derive(Default)]
struct Faux {
    base: String,
    /// Ce que rend `POST …/listen` : (statut, corps). Par défaut, un billet.
    reponse_listen: Option<(u16, Value)>,
    /// Ce que rend le pont : (statut, corps).
    reponse_pont: (u16, Value),
    /// Ce que rend la route locale de lecture.
    reponse_play: (u16, Value),
    /// Corps reçus par `POST …/listen`, avec l'en-tête Authorization.
    listens: Vec<(Option<String>, Value)>,
    /// `Range` de chaque appel au pont.
    sondes: Vec<Option<String>>,
    /// (zone, corps, en-têtes) de chaque `POST /zones/{id}/play`.
    plays: Vec<(String, Value, HeaderMap)>,
}

type Etat = Arc<Mutex<Faux>>;

fn billet(base: &str) -> Value {
    json!({
        "stream_url": format!("{base}/stream/circle/billet-5327"),
        "expires_at": "2026-09-28T12:05:00Z",
        "track": {
            "id": "42", "title": "So What", "artist_name": "Miles Davis",
            "album_title": "Kind of Blue", "format": "flac", "sample_rate": 96000,
            "bit_depth": 24, "duration_ms": 562000
        }
    })
}

async fn listen(
    State(e): State<Etat>,
    Path(_user_id): Path<String>,
    headers: HeaderMap,
    corps: Bytes,
) -> Response {
    let mut f = e.lock().unwrap();
    f.listens.push((
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        serde_json::from_slice(&corps).unwrap_or(Value::Null),
    ));
    let (statut, corps) = f
        .reponse_listen
        .clone()
        .unwrap_or_else(|| (200, billet(&f.base)));
    (StatusCode::from_u16(statut).unwrap(), Json(corps)).into_response()
}

async fn pont(State(e): State<Etat>, Path(_billet): Path<String>, headers: HeaderMap) -> Response {
    let mut f = e.lock().unwrap();
    f.sondes.push(
        headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
    );
    let (statut, corps) = f.reponse_pont.clone();
    if statut < 300 {
        return (
            StatusCode::from_u16(statut).unwrap(),
            [("content-type", "audio/flac")],
            b"f".to_vec(),
        )
            .into_response();
    }
    (StatusCode::from_u16(statut).unwrap(), Json(corps)).into_response()
}

async fn play(
    State(e): State<Etat>,
    Path(zone): Path<String>,
    headers: HeaderMap,
    Json(corps): Json<Value>,
) -> Response {
    let mut f = e.lock().unwrap();
    f.plays.push((zone, corps, headers));
    let (statut, corps) = f.reponse_play.clone();
    (StatusCode::from_u16(statut).unwrap(), Json(corps)).into_response()
}

async fn demarrer() -> (String, Etat) {
    let etat: Etat = Arc::new(Mutex::new(Faux {
        reponse_pont: (206, Value::Null),
        reponse_play: (200, json!({ "ok": true })),
        ..Default::default()
    }));
    let app = Router::new()
        .route("/api/v1/circle/contacts/{user_id}/listen", post(listen))
        .route("/stream/circle/{billet}", get(pont))
        .route("/api/v1/zones/{zone}/play", post(play))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    etat.lock().unwrap().base = base.clone();
    (base, etat)
}

fn reglages(base: &str, connecte: bool) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let s = SettingsRepo::with_backend(backend.clone());
    s.set("mozaik_base_url", base).unwrap();
    if connecte {
        s.set("mozaik_access_token", JETON).unwrap();
    }
    backend
}

fn greffon(base: &str, connecte: bool, bus: Option<EventBus>) -> (Router, Arc<Ecoute>) {
    let ecoute = Arc::new(Ecoute::new(
        Arc::new(Relais::new(reglages(base, connecte))),
        base,
        bus,
        None,
    ));
    (tune_circle::ecoute::router(ecoute.clone()), ecoute)
}

struct Rendu {
    statut: StatusCode,
    octets: Vec<u8>,
}

impl Rendu {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.octets).unwrap_or(Value::Null)
    }
}

async fn appel(app: &Router, methode: &str, chemin: &str, corps: Option<Value>) -> Rendu {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder()
        .method(methode)
        .uri(chemin)
        .header("authorization", JETON_TUNE)
        .header("x-profile-id", "2");
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(c.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let r = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Rendu { statut, octets }
}

fn ecoute_de(track: &str, zone: Value) -> Option<Value> {
    Some(json!({ "track_id": track, "zone_id": zone }))
}

// 1. Le chemin nominal -------------------------------------------------------

/// Billet délivré, pont ouvert : la lecture part sur LA zone demandée, en
/// `source: upnp` avec l'adresse du pont et les métadonnées, au nom de
/// l'appelant.
#[tokio::test]
async fn la_lecture_part_sur_la_bonne_zone_en_upnp_avec_les_metadonnees() {
    let (base, etat) = demarrer().await;
    let (app, _) = greffon(&base, true, None);

    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));

    let f = etat.lock().unwrap();
    // Le billet : `track_id` seul, avec le jeton SSO du serveur.
    assert_eq!(f.listens.len(), 1);
    assert_eq!(
        f.listens[0].0.as_deref(),
        Some(format!("Bearer {JETON}").as_str())
    );
    assert_eq!(f.listens[0].1, json!({ "track_id": "42" }));
    // La sonde : un octet.
    assert_eq!(f.sondes, vec![Some("bytes=0-0".to_string())]);
    // La lecture.
    assert_eq!(f.plays.len(), 1);
    let (zone, corps, entetes) = &f.plays[0];
    assert_eq!(zone, "3");
    assert_eq!(
        *corps,
        json!({
            "source": "upnp",
            "source_id": format!("{base}/stream/circle/billet-5327"),
            "title": "So What", "artist_name": "Miles Davis", "album_title": "Kind of Blue",
            "media_format": "flac", "sample_rate": 96000, "bit_depth": 24,
            "duration_ms": 562000
        })
    );
    assert_eq!(entetes["authorization"], JETON_TUNE);
    assert_eq!(entetes["x-profile-id"], "2");
}

// 2. Les refus du cloud, relayés tels quels ----------------------------------

#[tokio::test]
async fn les_refus_du_cloud_sont_relayes_tels_quels_et_rien_ne_joue() {
    for (statut, corps) in [
        (
            402,
            json!({ "error": "premium_required", "who": "listener" }),
        ),
        (404, json!({ "error": "not_found" })),
        (409, json!({ "error": "owner_unavailable" })),
        (429, json!({ "message": "Too Many Attempts." })),
    ] {
        let (base, etat) = demarrer().await;
        etat.lock().unwrap().reponse_listen = Some((statut, corps.clone()));
        let (app, _) = greffon(&base, true, None);

        let r = appel(
            &app,
            "POST",
            "/contacts/7/listen",
            ecoute_de("42", json!(3)),
        )
        .await;
        assert_eq!(r.statut.as_u16(), statut);
        assert_eq!(r.json(), corps, "{statut}");
        let f = etat.lock().unwrap();
        assert!(f.sondes.is_empty(), "{statut} : le pont a ete sonde");
        assert!(f.plays.is_empty(), "{statut} : une lecture est partie");
    }
}

#[tokio::test]
async fn cloud_en_panne_503_cloud_unavailable() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponse_listen = Some((500, json!({})));
    let (app, _) = greffon(&base, true, None);
    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["code"], "circle.cloud_unavailable");
    assert!(etat.lock().unwrap().plays.is_empty());
}

#[tokio::test]
async fn sans_session_412_et_rien_ne_part() {
    let (base, etat) = demarrer().await;
    let (app, _) = greffon(&base, false, None);
    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        r.json(),
        json!({ "connected": false, "code": "circle.not_connected" })
    );
    let f = etat.lock().unwrap();
    assert!(f.listens.is_empty() && f.plays.is_empty());
}

// 3. Ce que dit le pont --------------------------------------------------------

/// Le pont dit « serveur du contact éteint » : 503 `circle.owner_offline`, et
/// la zone n'est pas touchée.
#[tokio::test]
async fn serveur_du_contact_eteint_503_owner_offline() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponse_pont = (503, json!({ "code": "owner_offline" }));
    let (app, _) = greffon(&base, true, None);

    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.json(),
        json!({ "connected": true, "code": "circle.owner_offline" })
    );
    assert!(etat.lock().unwrap().plays.is_empty());
}

/// Billet délivré puis refusé par le pont (révoqué entre-temps) : la même 404
/// que « pas partagé ».
#[tokio::test]
async fn billet_refuse_par_le_pont_404() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponse_pont = (404, json!({ "code": "not_found" }));
    let (app, _) = greffon(&base, true, None);

    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "code": "circle.not_found" }));
    assert!(etat.lock().unwrap().plays.is_empty());
}

// 4. La demande elle-même ------------------------------------------------------

#[tokio::test]
async fn un_zone_id_qui_n_est_pas_un_entier_est_refuse_sans_appel() {
    let (base, etat) = demarrer().await;
    let (app, _) = greffon(&base, true, None);
    for zone in [json!(null), json!("3/../1"), json!("salon"), json!(1.5)] {
        let r = appel(
            &app,
            "POST",
            "/contacts/7/listen",
            ecoute_de("42", zone.clone()),
        )
        .await;
        assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "{zone}");
        assert_eq!(r.json()["code"], "circle.invalid_zone");
    }
    assert!(etat.lock().unwrap().listens.is_empty());
}

/// Un billet dont l'adresse n'est pas http(s) ne part jamais vers la zone.
#[tokio::test]
async fn un_billet_sans_adresse_http_ne_joue_pas() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponse_listen = Some((
        200,
        json!({ "stream_url": "file:///etc/passwd", "track": {} }),
    ));
    let (app, _) = greffon(&base, true, None);
    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::BAD_GATEWAY);
    let f = etat.lock().unwrap();
    assert!(f.sondes.is_empty() && f.plays.is_empty());
}

/// Le refus de la route de lecture (zone inconnue…) revient tel quel.
#[tokio::test]
async fn le_refus_de_la_route_de_lecture_est_relaye() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponse_play = (404, json!({ "error": "zone_not_found" }));
    let (app, _) = greffon(&base, true, None);
    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(99)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "zone_not_found" }));
}

// 5. Fin de partage en cours de lecture ----------------------------------------

async fn evenement_suivant(
    rx: &mut tokio::sync::broadcast::Receiver<TuneEvent>,
    nom: &str,
) -> Option<TuneEvent> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Ok(e) if e.event_type == nom => return e,
                Ok(_) => continue,
                Err(_) => std::future::pending::<()>().await,
            }
        }
    })
    .await
    .ok()
}

/// La zone qui joue le flux d'un contact tombe en erreur, et le pont refuse
/// désormais le billet : `circle.stream_revoked { zone_id }`.
#[tokio::test]
async fn un_billet_refuse_en_cours_de_lecture_emet_circle_stream_revoked() {
    let (base, etat) = demarrer().await;
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let (app, ecoute) = greffon(&base, true, Some(bus));

    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);

    // Révocation : le pont refuse la requête suivante.
    etat.lock().unwrap().reponse_pont = (404, json!({ "code": "not_found" }));
    ecoute
        .sur_evenement(&TuneEvent {
            event_type: "zone.playback_error".into(),
            data: json!({ "zone_id": 3, "error": "decode", "fatal": true }),
        })
        .await;

    let e = evenement_suivant(&mut rx, EVENEMENT_FIN_DE_PARTAGE)
        .await
        .expect("circle.stream_revoked n'est pas parti");
    assert_eq!(e.data, json!({ "zone_id": 3 }));
}

/// Une erreur sur une zone qui joue le flux, pont toujours ouvert : ce n'est
/// PAS une fin de partage, rien n'est émis. Une erreur sur une zone qui ne
/// joue pas de flux de contact : rien n'est même sondé.
#[tokio::test]
async fn une_autre_erreur_n_est_pas_une_fin_de_partage() {
    let (base, etat) = demarrer().await;
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let (app, ecoute) = greffon(&base, true, Some(bus));

    let r = appel(
        &app,
        "POST",
        "/contacts/7/listen",
        ecoute_de("42", json!(3)),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    let sondes_avant = etat.lock().unwrap().sondes.len();

    ecoute
        .sur_evenement(&TuneEvent {
            event_type: "zone.playback_error".into(),
            data: json!({ "zone_id": 5, "error": "x" }),
        })
        .await;
    ecoute
        .sur_evenement(&TuneEvent {
            event_type: "zone.playback_error".into(),
            data: json!({ "zone_id": 3, "error": "sortie absente" }),
        })
        .await;
    assert!(
        evenement_suivant(&mut rx, EVENEMENT_FIN_DE_PARTAGE)
            .await
            .is_none()
    );
    // Une seule sonde de plus : celle de la zone 3.
    assert_eq!(etat.lock().unwrap().sondes.len(), sondes_avant + 1);
}
