//! Les compteurs du pont vus de bout en bout (#6512) : une vraie requête
//! entre par le gestionnaire de route, un faux serveur Tune répond par les
//! trames qu'émet `tune-core/src/cloud/relay.rs`, et l'on compare la minute
//! relevée aux octets EXACTS qui ont circulé.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::state::RelayState;
use crate::trafic::{Envoi, MINUTES_PAR_ENVOI, pousser_au_site};

const JETON: &str = "jeton-du-pont-6512";
const PATIENCE: Duration = Duration::from_secs(5);

fn pont() -> (Arc<RelayState>, mpsc::Receiver<String>) {
    let state = Arc::new(RelayState::new());
    let (tx, rx) = mpsc::channel::<String>(16);
    state
        .register_server("srv".into(), "Salon".into(), JETON.into(), None, tx)
        .unwrap();
    (state, rx)
}

async fn recevoir(rx: &mut mpsc::Receiver<String>) -> String {
    tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("le pont n'a rien envoye au serveur")
        .expect("canal ferme")
}

/// Ce que le faux serveur a vu passer, et ce qu'il a émis, en octets.
#[derive(Default, Debug)]
struct Echange {
    /// Longueur du message reçu du pont.
    recu: usize,
    /// Longueurs des trames émises vers le pont.
    emis: Vec<usize>,
}

/// Le faux serveur : lit UNE requête de flux, puis émet `stream_start`, les
/// morceaux donnés (en base64) et, si `fin`, `relay.stream_end`.
fn serveur_de_flux(
    state: Arc<RelayState>,
    mut rx: mpsc::Receiver<String>,
    content_type: &'static str,
    status: u16,
    morceaux: Vec<&'static str>,
    fin: bool,
) -> tokio::task::JoinHandle<(Echange, mpsc::Receiver<String>)> {
    tokio::spawn(async move {
        let brut = recevoir(&mut rx).await;
        let msg: serde_json::Value = serde_json::from_str(&brut).unwrap();
        let id = msg["id"].as_str().unwrap().to_string();
        let mut e = Echange {
            recu: brut.len(),
            ..Default::default()
        };
        let mut trames = vec![
            serde_json::json!({
                "type": "relay.stream_start", "id": id, "status": status,
                "headers": { "content-type": content_type },
            })
            .to_string(),
        ];
        trames.extend(morceaux.iter().map(|m| format!("BINARY:{id}:{m}")));
        if fin {
            trames.push(serde_json::json!({"type": "relay.stream_end", "id": id}).to_string());
        }
        for trame in trames {
            e.emis.push(trame.len());
            crate::ws_server::handle_server_message(&state, "srv", &trame).await;
        }
        (e, rx)
    })
}

async fn flux(state: &Arc<RelayState>, chemin: &str, range: Option<&str>) -> Response {
    let mut h = HeaderMap::new();
    h.insert("x-bridge-token", HeaderValue::from_static(JETON));
    if let Some(r) = range {
        h.insert("range", HeaderValue::from_str(r).unwrap());
    }
    let uri: Uri = format!("/stream/relay/srv/{chemin}").parse().unwrap();
    tokio::time::timeout(
        PATIENCE,
        crate::stream_proxy::proxy_stream(
            State(state.clone()),
            Path(("srv".to_string(), chemin.to_string())),
            uri,
            h,
        ),
    )
    .await
    .expect("la route de flux ne repond pas")
}

async fn corps_entier(r: Response) -> Vec<u8> {
    tokio::time::timeout(PATIENCE, axum::body::to_bytes(r.into_body(), usize::MAX))
        .await
        .expect("le corps ne se termine pas")
        .unwrap()
        .to_vec()
}

/// LE témoin : chaque octet d'un flux audio est compté, dans le bon sens et
/// la bonne catégorie, et nulle part ailleurs.
#[tokio::test]
async fn les_octets_d_un_flux_audio_sont_comptes_exactement() {
    let (state, rx) = pont();
    // "fLaC" puis "\0\0\0\x22" : 8 octets de son.
    let serveur = serveur_de_flux(
        state.clone(),
        rx,
        "audio/flac",
        200,
        vec!["ZkxhQw==", "AAAAIg=="],
        true,
    );
    let r = flux(&state, "piste-6512.flac", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(corps_entier(r).await.len(), 8);
    let (e, _rx) = serveur.await.unwrap();

    let m = state.trafic.clore_minute(0);
    assert_eq!(
        m.categories.audio.r#in,
        e.emis.iter().sum::<usize>() as u64,
        "entree audio : les trames du serveur, base64 compris"
    );
    assert_eq!(
        m.categories.audio.out,
        8 + e.recu as u64,
        "sortie audio : le son rendu + la requete vers le serveur"
    );
    for (nom, c) in [
        ("api", &m.categories.api),
        ("circle", &m.categories.circle),
        ("images", &m.categories.images),
    ] {
        assert_eq!((c.r#in, c.out), (0, 0), "{nom} ne doit rien compter");
    }
    assert_eq!(m.sessions.started, 1);
    assert_eq!(m.sessions.completed, 1);
    assert_eq!(m.sessions.client_closed + m.sessions.server_lost, 0);
    assert_eq!(m.first_byte.count, 1);
    assert_eq!(m.servers.len(), 1);
    assert_eq!(m.servers[0].server_id, "srv");
    assert_eq!(m.servers[0].bytes_out, m.categories.audio.out);
    assert_eq!(m.servers[0].bytes_in, m.categories.audio.r#in);
    assert_eq!(m.servers[0].sessions, 1);
    assert_eq!(m.formats.len(), 1);
    assert_eq!(m.formats[0].format, "flac");
    assert_eq!(m.formats[0].bytes_out, 8);
    assert_eq!(m.formats[0].sessions, 1);
}

/// Le même témoin quand le serveur envoie ses morceaux en trames binaires
/// (#6564) : l'entrée audio compte les octets EXACTS des trames binaires,
/// en-tête compris, et non plus la longueur d'un base64 qui ne circule plus.
#[tokio::test]
async fn les_octets_d_un_flux_binaire_sont_comptes_exactement() {
    let (state, mut rx) = pont();
    let son: [&[u8]; 2] = [b"fLaC", b"\0\0\0\x22"];
    let serveur = {
        let state = state.clone();
        tokio::spawn(async move {
            let brut = recevoir(&mut rx).await;
            let msg: serde_json::Value = serde_json::from_str(&brut).unwrap();
            let id = msg["id"].as_str().unwrap().to_string();
            let debut = serde_json::json!({
                "type": "relay.stream_start", "id": id, "status": 200,
                "headers": { "content-type": "audio/flac" },
            })
            .to_string();
            let fin = serde_json::json!({"type": "relay.stream_end", "id": id}).to_string();
            let mut rangs = crate::morceau_binaire::Rangs::default();
            let mut emis = debut.len() + fin.len();
            crate::ws_server::handle_server_message(&state, "srv", &debut).await;
            for (seq, octets) in son.iter().enumerate() {
                // Le format de `crate::morceau_binaire`, écrit à la main.
                let mut trame = vec![1u8, id.len() as u8];
                trame.extend_from_slice(id.as_bytes());
                trame.extend_from_slice(&(seq as u64).to_be_bytes());
                trame.extend_from_slice(octets);
                emis += trame.len();
                crate::ws_server::handle_server_binary(&state, "srv", &trame, &mut rangs).await;
            }
            crate::ws_server::handle_server_message(&state, "srv", &fin).await;
            (brut.len(), emis, rx)
        })
    };
    let r = flux(&state, "piste-6564.flac", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(corps_entier(r).await, b"fLaC\0\0\0\x22");
    let (recu, emis, _rx) = serveur.await.unwrap();

    let m = state.trafic.clore_minute(0);
    assert_eq!(
        m.categories.audio.r#in, emis as u64,
        "entree audio : les trames binaires du serveur, en-tete compris"
    );
    assert_eq!(m.categories.audio.out, 8 + recu as u64);
    assert_eq!(
        (m.categories.api.r#in, m.categories.api.out),
        (0, 0),
        "un morceau binaire n'est pas de l'api"
    );
    assert_eq!(m.sessions.completed, 1);
    assert_eq!(m.servers[0].bytes_in, m.categories.audio.r#in);
}

/// Le navigateur part au milieu (saut de piste ou coupure) : la session finit
/// `client_closed`, seuls les octets REMIS sont comptés en sortie, et la
/// requête suivante du même flux avec un `Range` est une reprise.
#[tokio::test]
async fn un_navigateur_parti_puis_revenu_compte_une_coupure_et_une_reprise() {
    let (state, rx) = pont();
    let serveur = serveur_de_flux(state.clone(), rx, "audio/dsf", 200, vec!["ZkxhQw=="], false);
    let r = flux(&state, "piste-dsd.dsf", None).await;
    let mut corps = r.into_body().into_data_stream();
    let premier = tokio::time::timeout(PATIENCE, corps.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(premier.as_ref(), b"fLaC");
    drop(corps); // le navigateur ferme
    let (e, rx) = serveur.await.unwrap();

    let m = state.trafic.clore_minute(0);
    assert_eq!(m.sessions.started, 1);
    assert_eq!(m.sessions.client_closed, 1);
    assert_eq!(m.sessions.completed, 0);
    assert_eq!(m.categories.audio.out, 4 + e.recu as u64);
    assert_eq!(m.formats[0].format, "dsd");

    // Il revient, au milieu du fichier.
    let serveur = serveur_de_flux(state.clone(), rx, "audio/dsf", 206, vec![], true);
    let r = flux(&state, "piste-dsd.dsf", Some("bytes=4-")).await;
    let _ = corps_entier(r).await;
    let _ = serveur.await.unwrap();
    let m = state.trafic.clore_minute(60);
    assert_eq!(m.sessions.resumed, 1, "{m:?}");
    assert_eq!(m.servers[0].resumes, 1);
    assert_eq!(m.sessions.completed, 1);
}

/// Un autre flux, ou un `Range` qui part de zéro, n'est pas une reprise.
#[tokio::test]
async fn une_nouvelle_ecoute_n_est_pas_une_reprise() {
    let (state, rx) = pont();
    let serveur = serveur_de_flux(state.clone(), rx, "audio/flac", 200, vec![], false);
    let r = flux(&state, "a.flac", None).await;
    drop(r);
    let (_, rx) = serveur.await.unwrap();
    let serveur = serveur_de_flux(state.clone(), rx, "audio/flac", 206, vec![], true);
    let _ = corps_entier(flux(&state, "b.flac", Some("bytes=10-")).await).await;
    let (_, rx) = serveur.await.unwrap();
    let serveur = serveur_de_flux(state.clone(), rx, "audio/flac", 206, vec![], true);
    let _ = corps_entier(flux(&state, "a.flac", Some("bytes=0-")).await).await;
    let _ = serveur.await.unwrap();
    let m = state.trafic.clore_minute(0);
    assert_eq!(m.sessions.started, 3);
    assert_eq!(m.sessions.resumed, 0);
}

/// Le serveur Tune tombe en plein flux : `server_lost`, pas `completed`.
#[tokio::test]
async fn un_serveur_perdu_en_plein_flux_est_une_coupure_cote_serveur() {
    let (state, rx) = pont();
    let serveur = serveur_de_flux(
        state.clone(),
        rx,
        "audio/flac",
        200,
        vec!["ZkxhQw=="],
        false,
    );
    let r = flux(&state, "piste.flac", None).await;
    let _ = serveur.await.unwrap();
    state.unregister_server("srv");
    assert_eq!(corps_entier(r).await, b"fLaC".to_vec());
    let m = state.trafic.clore_minute(0);
    assert_eq!(m.sessions.server_lost, 1);
    assert_eq!(m.sessions.completed + m.sessions.client_closed, 0);
}

/// Un échec amont 5xx et un serveur absent sont des erreurs du pont.
#[tokio::test]
async fn les_erreurs_du_pont_sont_comptees() {
    let (state, rx) = pont();
    let serveur = serveur_de_flux(state.clone(), rx, "text/plain", 502, vec![], false);
    let r = flux(&state, "x.flac", None).await;
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    let _ = serveur.await.unwrap();

    let absent = crate::stream_proxy::proxy_stream(
        State(state.clone()),
        Path(("eteint".to_string(), "x.flac".to_string())),
        "/stream/relay/eteint/x.flac".parse().unwrap(),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);

    let m = state.trafic.clore_minute(0);
    assert_eq!(m.errors.upstream_5xx, 1);
    assert_eq!(m.errors.server_absent, 1);
    assert_eq!(
        m.sessions.started, 0,
        "un refus amont n'ouvre aucune session"
    );
}

/// Une réponse d'API, puis une pochette : `api` et `images`, au bon octet.
#[tokio::test]
async fn une_reponse_json_est_de_l_api_et_une_image_une_pochette() {
    let (state, mut rx) = pont();

    async fn appel(
        state: &Arc<RelayState>,
        rx: &mut mpsc::Receiver<String>,
        chemin: &str,
        corps_requete: &'static [u8],
        reponse: impl FnOnce(&str) -> serde_json::Value,
    ) -> (usize, usize, Vec<u8>) {
        let mut h = HeaderMap::new();
        h.insert("x-bridge-token", HeaderValue::from_static(JETON));
        let route = crate::api_proxy::proxy_api(
            State(state.clone()),
            Path(("srv".to_string(), chemin.to_string())),
            h,
            Method::POST,
            format!("/api/relay/srv/{chemin}").parse().unwrap(),
            axum::body::Bytes::from_static(corps_requete),
        );
        let route = tokio::spawn(route);
        let brut = recevoir(rx).await;
        let id = serde_json::from_str::<serde_json::Value>(&brut).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let trame = reponse(&id).to_string();
        crate::ws_server::handle_server_message(state, "srv", &trame).await;
        let r = tokio::time::timeout(PATIENCE, route)
            .await
            .unwrap()
            .unwrap();
        (brut.len(), trame.len(), corps_entier(r).await)
    }

    let (requete, trame, corps) = appel(&state, &mut rx, "library/albums", b"{\"q\":1}", |id| {
        serde_json::json!({"type": "relay.response", "id": id, "status": 200,
            "headers": {"content-type": "application/json"}, "body": "{\"ok\":true}"})
    })
    .await;
    assert_eq!(corps, b"{\"ok\":true}");
    let m = state.trafic.clore_minute(0);
    assert_eq!(m.categories.api.r#in, (7 + trame) as u64);
    assert_eq!(m.categories.api.out, (requete + 11) as u64);
    assert_eq!((m.categories.images.r#in, m.categories.images.out), (0, 0));

    let (requete, trame, corps) = appel(&state, &mut rx, "library/artwork/a.jpg", b"", |id| {
        serde_json::json!({"type": "relay.response", "id": id, "status": 200,
            "headers": {"content-type": "image/jpeg"}, "body_base64": "ZkxhQw=="})
    })
    .await;
    assert_eq!(corps, b"fLaC");
    let m = state.trafic.clore_minute(60);
    assert_eq!(m.categories.images.r#in, trame as u64);
    assert_eq!(m.categories.images.out, 4);
    // La requete vers le serveur reste de l'API : c'est la REPONSE qui est
    // une pochette.
    assert_eq!(m.categories.api.out, requete as u64);
    assert_eq!(m.categories.api.r#in, 0);
    assert_eq!(
        m.sessions.started, 0,
        "une reponse d'API n'est pas une ecoute"
    );
}

/// Deux serveurs : chacun ses octets.
#[tokio::test]
async fn chaque_serveur_a_ses_propres_comptes() {
    let state = Arc::new(RelayState::new());
    let (tx_a, _rx_a) = mpsc::channel::<String>(16);
    let (tx_b, _rx_b) = mpsc::channel::<String>(16);
    state
        .register_server("a".into(), "A".into(), "ja".into(), None, tx_a)
        .unwrap();
    state
        .register_server("b".into(), "B".into(), "jb".into(), None, tx_b)
        .unwrap();
    crate::ws_server::handle_server_message(&state, "a", "{\"type\":\"relay.pong\"}").await;
    crate::ws_server::handle_server_message(&state, "b", "{\"type\":\"relay.pong\"}").await;
    crate::ws_server::handle_server_message(&state, "b", "{\"type\":\"relay.pong\"}").await;
    let m = state.trafic.clore_minute(0);
    let par: std::collections::HashMap<_, _> = m
        .servers
        .iter()
        .map(|s| (s.server_id.as_str(), s.bytes_in))
        .collect();
    assert_eq!(par["a"], 21);
    assert_eq!(par["b"], 42);
    assert_eq!(m.categories.api.r#in, 63);
}

// --- L'envoi au site ---

#[derive(Default)]
struct FauxSite {
    statut: u16,
    appels: Vec<(Option<String>, serde_json::Value)>,
}

async fn recevoir_chez_le_faux(
    State(site): State<Arc<Mutex<FauxSite>>>,
    headers: HeaderMap,
    axum::Json(corps): axum::Json<serde_json::Value>,
) -> Response {
    let mut s = site.lock().unwrap();
    s.appels.push((
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        corps,
    ));
    StatusCode::from_u16(s.statut).unwrap().into_response()
}

async fn faux_site(statut: u16) -> (String, Arc<Mutex<FauxSite>>) {
    let site = Arc::new(Mutex::new(FauxSite {
        statut,
        appels: Vec::new(),
    }));
    let app = Router::new()
        .route(crate::trafic::ROUTE_DU_SITE, post(recevoir_chez_le_faux))
        .with_state(site.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("http://{adresse}"), site)
}

/// Accepté : les minutes partent, avec le jeton de service, et quittent la
/// file — par paquets bornés.
#[tokio::test]
async fn les_minutes_acceptees_par_le_site_quittent_la_file() {
    let (base, site) = faux_site(200).await;
    let t = crate::trafic::Trafic::nouveau();
    let n = MINUTES_PAR_ENVOI + 10;
    for i in 0..n as i64 {
        t.clore_minute(i * 60);
    }
    let client = reqwest::Client::new();
    let envoi = pousser_au_site(&t, &client, &base, Some("service-6512")).await;
    assert_eq!(envoi, Envoi::Accepte(n));
    assert_eq!(t.nombre_en_attente(), 0);
    let s = site.lock().unwrap();
    assert_eq!(s.appels.len(), 2, "deux paquets");
    assert_eq!(s.appels[0].0.as_deref(), Some("Bearer service-6512"));
    assert_eq!(
        s.appels[0].1["minutes"].as_array().unwrap().len(),
        MINUTES_PAR_ENVOI
    );
    assert_eq!(
        s.appels[1].1["minutes"][0]["minute"],
        MINUTES_PAR_ENVOI * 60
    );
}

/// Refusé (401, 404 tant que le site n'est pas déployé, 5xx) : rien ne se
/// perd, tout attend le prochain envoi.
#[tokio::test]
async fn un_refus_du_site_garde_les_minutes() {
    for statut in [401u16, 404, 500] {
        let (base, _site) = faux_site(statut).await;
        let t = crate::trafic::Trafic::nouveau();
        t.clore_minute(0);
        t.clore_minute(60);
        let envoi = pousser_au_site(&t, &reqwest::Client::new(), &base, Some("j")).await;
        assert_eq!(envoi, Envoi::Echec(Some(statut)));
        assert_eq!(t.nombre_en_attente(), 2, "statut {statut}");
    }
}

/// Sans jeton de service, rien ne part.
#[tokio::test]
async fn sans_jeton_rien_ne_part() {
    let (base, site) = faux_site(200).await;
    let t = crate::trafic::Trafic::nouveau();
    t.clore_minute(0);
    let envoi = pousser_au_site(&t, &reqwest::Client::new(), &base, None).await;
    assert_eq!(envoi, Envoi::Rien);
    assert_eq!(t.nombre_en_attente(), 1);
    assert!(site.lock().unwrap().appels.is_empty());
}
