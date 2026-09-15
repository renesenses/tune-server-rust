//! Abonnements GENA du ContentDirectory : une lecture du compteur par seconde,
//! indépendamment du nombre d'abonnés. Aucune tâche n'est lancée sans abonnement.
use axum::{
    Extension,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};
use tune_core::upnp_server::UpnpState;

const MAX_SUBSCRIBERS: usize = 64;
const MAX_TIMEOUT: u64 = 1800;

#[derive(Default)]
pub(super) struct Registry {
    entries: Mutex<HashMap<String, Subscriber>>,
    started: AtomicBool,
}

#[derive(Clone)]
struct Subscriber {
    callback: reqwest::Url,
    expires: Instant,
    revision: Option<u32>,
    seq: u32,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn timeout(headers: &HeaderMap) -> Result<u64, StatusCode> {
    match header(headers, "TIMEOUT") {
        None | Some("Second-infinite") => Ok(MAX_TIMEOUT),
        Some(value) => value
            .strip_prefix("Second-")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(|v| v.min(MAX_TIMEOUT))
            .ok_or(StatusCode::BAD_REQUEST),
    }
}

/// Un callback ne peut cibler que l'adresse IP du client TCP. Aucun proxy,
/// DNS ni redirect : un abonnement ne permet pas de sonder d'autres machines.
fn callback(headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<reqwest::Url> {
    let raw = header(headers, "CALLBACK")?;
    if raw.len() > 2048 {
        return None;
    }
    let url = reqwest::Url::parse(raw.strip_prefix('<')?.strip_suffix('>')?).ok()?;
    let ip: std::net::IpAddr = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()?;
    (url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && ip.to_canonical() == peer?.ip().to_canonical())
    .then_some(url)
}

pub(super) async fn subscription(
    State(state): State<UpnpState>,
    Extension(registry): Extension<Arc<Registry>>,
    request: Request,
) -> Response {
    let headers = request.headers();
    let sid = header(headers, "SID");
    let method = request.method().as_str();
    if method != "SUBSCRIBE" && method != "UNSUBSCRIBE" {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let mut entries = registry.entries.lock().await;
    entries.retain(|_, entry| entry.expires > Instant::now());
    if let Some(sid) = sid {
        if headers.contains_key("CALLBACK") || headers.contains_key("NT") {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Some(entry) = entries.get_mut(sid) else {
            return StatusCode::PRECONDITION_FAILED.into_response();
        };
        if method == "UNSUBSCRIBE" {
            entries.remove(sid);
            return StatusCode::OK.into_response();
        }
        let seconds = match timeout(headers) {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };
        entry.expires = Instant::now() + Duration::from_secs(seconds);
        return subscribed(sid, seconds);
    }
    if method == "UNSUBSCRIBE" || header(headers, "NT") != Some("upnp:event") {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let Some(callback) = callback(headers, peer) else {
        return StatusCode::PRECONDITION_FAILED.into_response();
    };
    let seconds = match timeout(headers) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    if entries.len() >= MAX_SUBSCRIBERS {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let sid = tune_core::upnp_server::new_subscription_sid();
    entries.insert(
        sid.clone(),
        Subscriber {
            callback,
            expires: Instant::now() + Duration::from_secs(seconds),
            revision: None,
            seq: 0,
        },
    );
    drop(entries);
    if !registry.started.swap(true, Ordering::AcqRel) {
        tokio::spawn(run(Arc::downgrade(&registry), state));
    }
    subscribed(&sid, seconds)
}

fn subscribed(sid: &str, seconds: u64) -> Response {
    (
        [
            ("SID", sid.to_owned()),
            ("TIMEOUT", format!("Second-{seconds}")),
        ],
        "",
    )
        .into_response()
}

async fn run(weak: Weak<Registry>, state: UpnpState) {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("UPnP event client: {e}");
            if let Some(registry) = weak.upgrade() {
                registry.started.store(false, Ordering::Release);
            }
            return;
        }
    };
    let mut initial = true;
    loop {
        // Premier événement avant l'expiration d'un abonnement court ; les
        // contrôles suivants restent espacés d'au moins une seconde.
        let delay = if initial {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(1)
        };
        initial = false;
        tokio::time::sleep(delay).await;
        let Some(registry) = weak.upgrade() else {
            return;
        };
        {
            let mut entries = registry.entries.lock().await;
            entries.retain(|_, entry| entry.expires > Instant::now());
            if entries.is_empty() {
                // Sous le verrou pour ne pas perdre un abonnement concurrent.
                registry.started.store(false, Ordering::Release);
                return;
            }
        }
        let db = state.backend.clone();
        let revision = match tokio::task::spawn_blocking(move || {
            tune_core::db::upnp_revision::read(db.as_ref())
        })
        .await
        {
            Ok(Ok(value)) => value,
            _ => continue, // Ne jamais notifier une valeur inventée.
        };
        let pending: Vec<_> = registry
            .entries
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| entry.revision != Some(revision))
            .map(|(sid, entry)| (sid.clone(), entry.clone()))
            .collect();
        stream::iter(pending).for_each_concurrent(4, |(sid, entry)| {
            let client = &client;
            let registry = &registry;
            async move {
                if !registry.entries.lock().await.get(&sid).is_some_and(|e| e.expires > Instant::now()) { return; }
                let body = format!("<?xml version=\"1.0\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property><SystemUpdateID>{revision}</SystemUpdateID></e:property></e:propertyset>");
                let sent = client.request(reqwest::Method::from_bytes(b"NOTIFY").unwrap(), entry.callback)
                    .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
                    .header("NT", "upnp:event").header("NTS", "upnp:propchange")
                    .header("SID", &sid).header("SEQ", entry.seq.to_string())
                    .body(body).send().await.is_ok_and(|r| r.status().is_success());
                if let Some(current) = registry.entries.lock().await.get_mut(&sid) {
                    // SEQ 0 réservé au premier envoi ; le débordement revient à 1.
                    current.seq = entry.seq.wrapping_add(1).max(1);
                    if sent { current.revision = Some(revision); }
                }
            }
        }).await;
        // registry est lâché avant le sleep : la destruction du routeur arrête le worker.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::any};
    use tower::ServiceExt;
    use tune_core::db::{backend::DbBackend, migrations, sqlite::SqliteDb};

    fn database() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    async fn call(router: &Router, method: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri("/ContentDirectory/event");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
        ));
        router.clone().oneshot(request).await.unwrap()
    }

    fn router(db: Arc<dyn DbBackend>) -> Router {
        super::super::standalone_router(UpnpState {
            backend: db,
            server_port: 8888,
            friendly_name: "test".into(),
            uuid: "test".into(),
            advertised_ip: None,
        })
    }

    #[tokio::test]
    async fn system_update_id_gena_notifie_modification_renouvelle_et_desabonne() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let callback_app = Router::new().route(
            "/notify",
            any(move |headers: HeaderMap, body: String| {
                let sender = sender.clone();
                async move {
                    sender.send((headers, body)).unwrap();
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let callback_url = format!("<http://{}/notify>", listener.local_addr().unwrap());
        let task = tokio::spawn(async {
            axum::serve(listener, callback_app).await.unwrap();
        });
        let db = database();
        let app = router(db.clone());
        let response = call(
            &app,
            "SUBSCRIBE",
            &[
                ("CALLBACK", &callback_url),
                ("NT", "upnp:event"),
                ("TIMEOUT", "Second-1"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let sid = response.headers()["SID"].to_str().unwrap().to_owned();
        let (headers, body) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("le SUBSCRIBE doit produire un NOTIFY initial")
            .unwrap();
        assert_eq!(headers["SID"], sid);
        assert_eq!(headers["SEQ"], "0");
        assert_eq!(headers["NT"], "upnp:event");
        assert_eq!(headers["NTS"], "upnp:propchange");
        let first = tune_core::db::upnp_revision::read(db.as_ref()).unwrap();
        assert!(body.contains(&format!("<SystemUpdateID>{first}</SystemUpdateID>")));
        assert_eq!(
            call(
                &app,
                "SUBSCRIBE",
                &[("SID", &sid), ("TIMEOUT", "Second-600")]
            )
            .await
            .status(),
            StatusCode::OK,
            "le NOTIFY initial doit arriver avant expiration du SID court"
        );
        db.execute_batch("INSERT INTO tracks (id,title) VALUES (87654,'Nouvelle piste')")
            .unwrap();
        let (headers, body) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("une modification du catalogue doit produire un NOTIFY")
            .unwrap();
        let next = tune_core::db::upnp_revision::read(db.as_ref()).unwrap();
        assert_ne!(next, first);
        assert_eq!(headers["SEQ"], "1");
        assert!(
            body.contains(&format!("<SystemUpdateID>{next}</SystemUpdateID>")),
            "notification périmée : {body}"
        );
        let renewed = call(
            &app,
            "SUBSCRIBE",
            &[("SID", &sid), ("TIMEOUT", "Second-600")],
        )
        .await;
        assert_eq!(renewed.status(), StatusCode::OK);
        assert_eq!(renewed.headers()["SID"], sid);
        assert_eq!(renewed.headers()["TIMEOUT"], "Second-600");
        db.execute_batch("UPDATE tracks SET title = title WHERE id = 87654")
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1300), receiver.recv())
                .await
                .is_err(),
            "ni renouvellement ni écriture identique ne doit notifier"
        );
        assert_eq!(
            call(&app, "UNSUBSCRIBE", &[("SID", &sid)]).await.status(),
            StatusCode::OK
        );
        db.execute_batch("DELETE FROM tracks WHERE id = 87654")
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1300), receiver.recv())
                .await
                .is_err(),
            "abonnement supprimé encore notifié"
        );
        assert_eq!(
            call(&app, "SUBSCRIBE", &[("SID", &sid)]).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
        task.abort();
    }

    #[tokio::test]
    async fn system_update_id_gena_valide_callback_expiration_et_sid() {
        let app = router(database());
        for address in [
            "<http://192.0.2.1:1234/notify>",
            "<http://localhost:1234/notify>",
            "<http://user@127.0.0.1/notify>",
            "<https://127.0.0.1/notify>",
        ] {
            assert_eq!(
                call(
                    &app,
                    "SUBSCRIBE",
                    &[("CALLBACK", address), ("NT", "upnp:event")]
                )
                .await
                .status(),
                StatusCode::PRECONDITION_FAILED,
                "callback non autorisé : {address}"
            );
        }
        let response = call(
            &app,
            "SUBSCRIBE",
            &[
                ("CALLBACK", "<http://127.0.0.1:9/notify>"),
                ("NT", "upnp:event"),
                ("TIMEOUT", "Second-1"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let sid = response.headers()["SID"].to_str().unwrap();
        assert_eq!(
            call(&app, "SUBSCRIBE", &[("SID", sid), ("NT", "upnp:event")])
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            call(&app, "SUBSCRIBE", &[("SID", sid)]).await.status(),
            StatusCode::PRECONDITION_FAILED,
            "un SID expiré ne se renouvelle pas"
        );
    }
}
