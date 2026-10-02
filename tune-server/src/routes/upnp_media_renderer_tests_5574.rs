//! #5574 — JPlay pilote la façade MediaRenderer d'une zone avec une URL
//! DISTANTE (onglet Qobuz de JPlay), pas une piste de la bibliothèque.
//!
//! Le cas de terrain (Tades, fil 2070, zone « DDC-0 C19 », Tune OS) : la
//! télécommande « fait semblant de lire Qobuz mais aucun son ne sort », et
//! Tune affiche « Le renderer a acquitté Play mais joue toujours une autre
//! source (il tient encore : …/stream/<uuid>.wav) ».
//!
//! Le banc : un faux point de contrôle (requêtes SOAP brutes, comme JPlay)
//! contre le routeur de production, un faux serveur HTTP qui sert l'URL
//! distante, et une sortie de zone factice (`MockOutput`) qui peut refuser la
//! lecture comme le renderer DLNA de la C19.
use super::*;
use axum::http::Request;
use std::sync::atomic::{AtomicI64, Ordering};
use tower::ServiceExt;

/// Le flux Tune que la zone tient avant la commande JPlay (toast d'origine).
const ANCIEN_FLUX: &str = "http://127.0.0.1:9/stream/4efa806c-9f15-4ef6-8ee0-956ee206db96.wav";

/// Le motif exact que rend `DlnaOutput::play_media` quand le renderer acquitte
/// Play mais tient encore l'ancien flux (`outputs/dlna.rs`).
const REFUS_DLNA: &str = "Le renderer a acquitté Play mais joue toujours une autre source \
     (il tient encore : http://127.0.0.1:9/stream/4efa806c-9f15-4ef6-8ee0-956ee206db96.wav)";

struct Banc {
    state: AppState,
    app: Router,
    zone: i64,
    device: String,
    /// Base du faux serveur qui sert l'URL distante.
    distant: String,
    serveur: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

/// Échappement XML d'un contenu d'élément, comme un point de contrôle.
fn echapper(texte: &str) -> String {
    texte
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

struct Reponse {
    status: StatusCode,
    corps: String,
}

impl Banc {
    async fn new() -> Self {
        // `sessions()` est une statique de processus : une plage d'ids à nous.
        static ZONE: AtomicI64 = AtomicI64::new(5_574_000);
        let zone = ZONE.fetch_add(1, Ordering::Relaxed);
        let device = format!("mock-5574-{zone}");
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("renderer.db");
        let config = crate::config::TuneConfig {
            db_path: db.to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let state = AppState::new(db.to_str().unwrap(), 0, config).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
                &[&zone, &"DDC-0 C19", &"mock", &device],
            )
            .unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("zone_{zone}_upnp_renderer"), "true")
            .unwrap();
        state.outputs.lock().await.register(Box::new(
            tune_core::outputs::mock::MockOutput::new(&device, "C19").with_type("dlna"),
        ));
        let app = crate::routes::router(state.clone());

        // Le faux serveur distant : quelques octets FLAC, sans extension
        // d'URL, comme un CDN de service.
        let distant_app = Router::new().route(
            "/file",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "audio/flac")],
                    b"fLaC\0\0\0\x22".to_vec(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serveur = tokio::spawn(async move {
            let _ = axum::serve(listener, distant_app).await;
        });
        Self {
            state,
            app,
            zone,
            device,
            distant: format!("http://{addr}"),
            serveur,
            _dir: dir,
        }
    }

    /// Une URL de service : requête signée, `&` multiples, aucune extension.
    fn url_distante(&self) -> String {
        format!(
            "{}/file?uid=881&eid=123456789&fmt=27&profile=raw&etsp=1790000000&hmac=abc",
            self.distant
        )
    }

    async fn soap(&self, action: &str, args: &str) -> Reponse {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"#
        );
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(format!("/upnp/renderer/{}/AVTransport/control", self.zone))
                    .header("content-type", "text/xml; charset=\"utf-8\"")
                    .header(
                        "soapaction",
                        format!("\"urn:schemas-upnp-org:service:AVTransport:1#{action}\""),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Reponse {
            status,
            corps: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// `SetAVTransportURI` tel que JPlay l'envoie : URI échappée XML, DIDL
    /// échappé.
    async fn poser(&self, uri: &str, titre: &str) {
        let uri = echapper(uri);
        let didl = echapper(&format!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1" parentID="0" restricted="1"><dc:title>{titre}</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo="http-get:*:audio/flac:*" duration="0:04:49.000">{uri}</res></item></DIDL-Lite>"#
        ));
        let r = self
            .soap(
                "SetAVTransportURI",
                &format!(
                    "<CurrentURI>{uri}</CurrentURI><CurrentURIMetaData>{didl}</CurrentURIMetaData>"
                ),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "SetAVTransportURI : {}", r.corps);
    }

    async fn play(&self) -> Reponse {
        self.soap("Play", "<Speed>1</Speed>").await
    }

    async fn transport_state(&self) -> String {
        let r = self.soap("GetTransportInfo", "").await;
        assert_eq!(r.status, StatusCode::OK, "GetTransportInfo : {}", r.corps);
        let debut = r.corps.find("<CurrentTransportState>").unwrap() + 23;
        let fin = r.corps[debut..].find('<').unwrap();
        r.corps[debut..debut + fin].to_string()
    }

    async fn derniere_url_envoyee(&self) -> Option<String> {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .last_play_url()
            .await
    }

    async fn faire_refuser_la_lecture(&self, motif: &str) {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .refuser_la_lecture(Some(motif));
    }
}

/// La partie TUNE du relais : la commande JPlay remplace bien le flux en
/// cours. La session du renderer prend l'URL distante, l'orchestrateur la
/// remet à la sortie de la zone telle quelle (requête signée intacte), et la
/// lecture en cours la nomme. Si la C19 garde l'ancien flux, ce n'est pas
/// faute que Tune le lui ait demandé.
#[tokio::test]
async fn une_url_distante_de_jplay_remplace_le_flux_tenu_par_la_zone() {
    let b = Banc::new().await;
    b.poser(ANCIEN_FLUX, "Piste locale").await;
    let r = b.play().await;
    assert_eq!(r.status, StatusCode::OK, "Play initial : {}", r.corps);

    let distante = b.url_distante();
    b.poser(&distante, "Titre Qobuz").await;
    let r = b.play().await;
    assert_eq!(r.status, StatusCode::OK, "Play distant : {}", r.corps);

    let envoyee = b.derniere_url_envoyee().await;
    assert_eq!(
        envoyee.as_deref(),
        Some(distante.as_str()),
        "la sortie de la zone doit recevoir l'URL distante, pas l'ancien flux"
    );
    let np = b
        .state
        .playback
        .get_state(b.zone)
        .await
        .now_playing
        .unwrap();
    assert_eq!(np.source, "upnp");
    assert_eq!(np.source_id.as_deref(), Some(distante.as_str()));
    assert_eq!(np.title, "Titre Qobuz");
    assert_eq!(b.transport_state().await, "PLAYING");
}

/// Le défaut côté façade : quand la sortie REFUSE (renderer DLNA qui
/// acquitte Play et garde l'ancien flux), `play()` rend `Ok` avec l'erreur
/// dans le résultat — et la façade acquittait `Play`. JPlay croyait jouer :
/// « elle fait semblant de lire Qobuz mais aucun son ne sort ».
#[tokio::test]
async fn un_play_refuse_par_la_sortie_n_est_pas_acquitte_au_point_de_controle() {
    let b = Banc::new().await;
    b.poser(ANCIEN_FLUX, "Piste locale").await;
    assert_eq!(b.play().await.status, StatusCode::OK);

    b.faire_refuser_la_lecture(REFUS_DLNA).await;
    b.poser(&b.url_distante(), "Titre Qobuz").await;
    let r = b.play().await;

    assert_eq!(
        r.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "#5574 : un Play que la sortie a refusé ne doit pas être acquitté : {}",
        r.corps
    );
    assert!(
        r.corps.contains("<errorCode>701</errorCode>"),
        "le refus doit être une faute UPnP 701 : {}",
        r.corps
    );
    assert!(
        r.corps.contains("joue toujours une autre source"),
        "le motif de la sortie doit voyager jusqu'au point de contrôle : {}",
        r.corps
    );
    assert_ne!(
        b.transport_state().await,
        "PLAYING",
        "après un refus, la façade ne doit pas annoncer une lecture"
    );
}
