//! Banc du repli conservateur sur un refus de `SetAVTransportURI`.
//!
//! Le faux renderer imite une pile `upmpdcli` dont le contrôle
//! `checkcontentformat` est actif : il lit le MIME du `protocolInfo` de la
//! DIDL, et tout MIME absent de sa liste reçoit `501 Action Failed` — la
//! réponse générique de libupnp. Il publie sa liste dans
//! `GetProtocolInfo` → `Sink`, comme le vrai. Selon le banc, il accepte ou
//! non une pose aux métadonnées vides.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use tokio::sync::Mutex;

use crate::outputs::dlna::DlnaOutput;
use crate::outputs::dlna_repli_set_uri as repli;
use crate::outputs::traits::{OutputTarget, PlayMedia};

#[derive(Clone)]
struct Strict {
    /// MIME (minuscules) que le renderer accepte dans le protocolInfo.
    acceptes: Arc<Mutex<Vec<String>>>,
    /// Ce qu'il publie dans son Sink.
    sink: Arc<Vec<String>>,
    /// Code de la faute rendue sur un refus (501 par défaut).
    code_refus: u16,
    /// Accepte une pose aux métadonnées vides.
    accepte_sans_meta: bool,
    /// Toutes les actions AVTransport reçues, dans l'ordre.
    actions: Arc<Mutex<Vec<String>>>,
    set_uri: Arc<Mutex<Vec<String>>>,
    set_next: Arc<Mutex<Vec<String>>>,
    get_protocol_info: Arc<AtomicU32>,
    current_uri: Arc<Mutex<String>>,
}

impl Strict {
    fn new(acceptes: &[&str], sink: &[&str], code_refus: u16) -> Self {
        Self {
            acceptes: Arc::new(Mutex::new(acceptes.iter().map(|m| m.to_string()).collect())),
            sink: Arc::new(sink.iter().map(|s| s.to_string()).collect()),
            code_refus,
            accepte_sans_meta: false,
            actions: Arc::default(),
            set_uri: Arc::default(),
            set_next: Arc::default(),
            get_protocol_info: Arc::default(),
            current_uri: Arc::default(),
        }
    }
}

fn enveloppe(corps: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{corps}</s:Body></s:Envelope>"#
    )
}

fn faute(code: u16, description: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        enveloppe(&format!(
            r#"<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault>"#
        )),
    )
        .into_response()
}

fn action(corps: &str) -> String {
    // `<u:Action xmlns:u=...>` : le nom suit `<u:`.
    corps
        .split("<u:")
        .nth(1)
        .and_then(|r| r.split([' ', '>']).next())
        .unwrap_or_default()
        .to_string()
}

fn tag(corps: &str, nom: &str) -> String {
    crate::outputs::dlna::extract_tag(corps, nom).unwrap_or_default()
}

/// Le MIME du protocolInfo d'une DIDL échappée, comme le lit upmpdcli.
fn mime_annonce(corps: &str) -> Option<String> {
    repli::protocol_info_du_didl(corps)
        .and_then(|p| p.split(':').nth(2).map(|m| m.to_ascii_lowercase()))
}

async fn av(State(s): State<Strict>, corps: String) -> axum::response::Response {
    s.actions.lock().await.push(action(&corps));
    match action(&corps).as_str() {
        "SetAVTransportURI" => {
            s.set_uri.lock().await.push(corps.clone());
            let ok = match mime_annonce(&corps) {
                Some(m) => s.acceptes.lock().await.contains(&m),
                None => s.accepte_sans_meta && tag(&corps, "CurrentURIMetaData").is_empty(),
            };
            if !ok {
                return faute(s.code_refus, "Action Failed");
            }
            *s.current_uri.lock().await = tag(&corps, "CurrentURI");
            enveloppe("<u:SetAVTransportURIResponse/>").into_response()
        }
        "SetNextAVTransportURI" => {
            s.set_next.lock().await.push(corps.clone());
            let ok = match mime_annonce(&corps) {
                Some(m) => s.acceptes.lock().await.contains(&m),
                None => false,
            };
            if !ok {
                return faute(s.code_refus, "Action Failed");
            }
            enveloppe("<u:SetNextAVTransportURIResponse/>").into_response()
        }
        "GetTransportInfo" => enveloppe(
            "<u:GetTransportInfoResponse><CurrentTransportState>STOPPED</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse>",
        )
        .into_response(),
        "GetMediaInfo" => {
            let uri = s.current_uri.lock().await.clone();
            enveloppe(&format!(
                "<u:GetMediaInfoResponse><CurrentURI>{uri}</CurrentURI></u:GetMediaInfoResponse>"
            ))
            .into_response()
        }
        "GetPositionInfo" => {
            let uri = s.current_uri.lock().await.clone();
            enveloppe(&format!(
                "<u:GetPositionInfoResponse><TrackURI>{uri}</TrackURI><RelTime>0:00:00</RelTime><TrackDuration>0:03:00</TrackDuration></u:GetPositionInfoResponse>"
            ))
            .into_response()
        }
        autre => enveloppe(&format!("<u:{autre}Response/>")).into_response(),
    }
}

async fn cm(State(s): State<Strict>, corps: String) -> axum::response::Response {
    if action(&corps) == "GetProtocolInfo" {
        s.get_protocol_info.fetch_add(1, Ordering::Relaxed);
        return enveloppe(&format!(
            "<u:GetProtocolInfoResponse><Source></Source><Sink>{}</Sink></u:GetProtocolInfoResponse>",
            s.sink.join(",")
        ))
        .into_response();
    }
    enveloppe("<u:Response/>").into_response()
}

async fn demarrer(etat: Strict, udn: &str) -> (DlnaOutput, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/av", post(av))
        .route("/rc", post(|| async { enveloppe("<u:R/>") }))
        .route("/cm", post(cm))
        .with_state(etat);
    let vie = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let sortie = DlnaOutput::new(
        "Renderer strict".into(),
        udn.into(),
        "127.0.0.1".into(),
        format!("http://127.0.0.1:{port}/av"),
        format!("http://127.0.0.1:{port}/rc"),
        Some(format!("http://127.0.0.1:{port}/cm")),
    );
    (sortie, vie)
}

fn piste(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        title: Some("Piste"),
        artist: Some("Artiste"),
        album: Some("Album"),
        duration_ms: Some(180_000),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        channels: Some(2),
        ..Default::default()
    }
}

/// Le cas de la pile upmpdcli : elle ne publie que `audio/x-flac` et refuse
/// `audio/flac` en 501. Un seul repli (orthographe du Sink, DIDL minimale)
/// suffit, et il est MÉMORISÉ : la piste suivante et le gapless partent
/// directement du bon profil.
#[tokio::test]
async fn un_501_se_reprend_sur_l_orthographe_du_sink_et_se_memorise() {
    let udn = "uuid:banc-repli-501-orthographe";
    repli::oublier_profil(udn, "audio/flac");
    let etat = Strict::new(
        &["audio/x-flac"],
        &["http-get:*:audio/x-flac:*", "http-get:*:audio/wav:*"],
        501,
    );
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/repli-501-a.flac"))
        .await
        .expect("le repli sur audio/x-flac doit passer");
    {
        let poses = etat.set_uri.lock().await;
        assert_eq!(poses.len(), 2, "un refus puis un repli, pas plus");
        assert_eq!(mime_annonce(&poses[0]).as_deref(), Some("audio/flac"));
        assert_eq!(mime_annonce(&poses[1]).as_deref(), Some("audio/x-flac"));
        assert!(
            !poses[1].contains("albumArtURI") && !poses[1].contains("upnp:album"),
            "le repli part en DIDL minimale"
        );
    }
    assert_eq!(
        etat.get_protocol_info.load(Ordering::Relaxed),
        1,
        "une seule sonde du Sink"
    );
    assert_eq!(
        repli::profil_memorise(udn, "audio/flac").map(|p| p.mime_annonce),
        Some("audio/x-flac".to_string())
    );

    // Piste suivante : le profil appris part du premier coup.
    etat.set_uri.lock().await.clear();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/repli-501-b.flac"))
        .await
        .expect("profil mémorisé");
    {
        let poses = etat.set_uri.lock().await;
        assert_eq!(poses.len(), 1, "aucun refus repayé : {}", poses.len());
        assert_eq!(mime_annonce(&poses[0]).as_deref(), Some("audio/x-flac"));
    }
    assert_eq!(etat.get_protocol_info.load(Ordering::Relaxed), 1);

    // Gapless : même profil.
    sortie
        .set_next_media(&piste("http://127.0.0.1:9/stream/repli-501-c.flac"))
        .await
        .expect("la suivante prend le profil appris");
    let suivantes = etat.set_next.lock().await;
    assert_eq!(suivantes.len(), 1);
    assert_eq!(mime_annonce(&suivantes[0]).as_deref(), Some("audio/x-flac"));
    drop(suivantes);
    repli::oublier_profil(udn, "audio/flac");
    vie.abort();
}

/// Un renderer qui refuse toute DIDL, quelle qu'en soit la forme, mais
/// accepte une pose sans métadonnées : le second palier (métadonnées vides)
/// passe, chaque repli est précédé d'un Stop, et le palier est mémorisé.
#[tokio::test]
async fn un_501_va_jusqu_aux_metadonnees_vides_et_s_en_souvient() {
    let udn = "uuid:banc-repli-501-sans-meta";
    repli::oublier_profil(udn, "audio/flac");
    let mut etat = Strict::new(&[], &["http-get:*:audio/flac:*"], 501);
    etat.accepte_sans_meta = true;
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/repli-sans-meta-a.flac"))
        .await
        .expect("le second palier (métadonnées vides) doit passer");
    {
        let poses = etat.set_uri.lock().await;
        assert_eq!(poses.len(), 3, "complet, minimal canonique, vide");
        assert!(poses[0].contains("upnp:album"), "1re pose : DIDL complète");
        assert!(
            mime_annonce(&poses[1]).is_some() && !poses[1].contains("upnp:album"),
            "2e pose : DIDL minimale"
        );
        assert!(
            tag(&poses[2], "CurrentURIMetaData").is_empty(),
            "3e pose : vide"
        );
    }
    {
        // Chaque repli est précédé d'un Stop.
        let actions = etat.actions.lock().await;
        let poses: Vec<usize> = actions
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "SetAVTransportURI")
            .map(|(i, _)| i)
            .collect();
        for fenetre in poses.windows(2) {
            assert!(
                actions[fenetre[0]..fenetre[1]].iter().any(|a| a == "Stop"),
                "pas de Stop entre deux poses : {actions:?}"
            );
        }
    }
    assert_eq!(
        repli::profil_memorise(udn, "audio/flac").map(|p| p.niveau_didl_min),
        Some(2)
    );
    etat.set_uri.lock().await.clear();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/repli-sans-meta-b.flac"))
        .await
        .expect("palier mémorisé");
    let poses = etat.set_uri.lock().await;
    assert_eq!(poses.len(), 1, "le palier appris part du premier coup");
    assert!(tag(&poses[0], "CurrentURIMetaData").is_empty());
    drop(poses);
    repli::oublier_profil(udn, "audio/flac");
    vie.abort();
}

/// 716 suit la même reprise que 501.
#[tokio::test]
async fn un_716_suit_la_meme_reprise() {
    let udn = "uuid:banc-repli-716";
    repli::oublier_profil(udn, "audio/flac");
    let etat = Strict::new(&["audio/x-flac"], &["http-get:*:audio/x-flac:*"], 716);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/repli-716.flac"))
        .await
        .expect("716 : repli sur l'orthographe du Sink");
    assert_eq!(etat.set_uri.lock().await.len(), 2);
    repli::oublier_profil(udn, "audio/flac");
    vie.abort();
}

/// Pas de boucle : un renderer qui refuse TOUT voit trois poses (l'originale
/// et deux replis), une seule sonde du Sink, et l'erreur remonte avec son 501.
/// Rien n'est mémorisé.
#[tokio::test]
async fn un_renderer_qui_refuse_tout_voit_deux_replis_au_plus() {
    let udn = "uuid:banc-repli-refus-total";
    repli::oublier_profil(udn, "audio/flac");
    let etat = Strict::new(
        &[],
        &["http-get:*:audio/x-flac:*", "http-get:*:audio/wav:*"],
        501,
    );
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    let err = sortie
        .play_media(&piste("http://127.0.0.1:9/stream/refus-total.flac"))
        .await
        .expect_err("tout est refusé");
    assert!(err.contains("SetAVTransportURI rejected"), "{err}");
    assert!(err.contains("<errorCode>501</errorCode>"), "{err}");
    assert_eq!(etat.set_uri.lock().await.len(), 3, "deux replis au plus");
    assert_eq!(etat.get_protocol_info.load(Ordering::Relaxed), 1);
    assert_eq!(repli::profil_memorise(udn, "audio/flac"), None);
    vie.abort();
}

/// Contre-épreuve : un renderer sain n'en paie rien — une pose, aucune sonde
/// du Sink, rien de mémorisé.
#[tokio::test]
async fn un_renderer_sain_ne_paie_rien() {
    let udn = "uuid:banc-repli-sain";
    repli::oublier_profil(udn, "audio/flac");
    let etat = Strict::new(&["audio/flac"], &["http-get:*:audio/flac:*"], 501);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/sain.flac"))
        .await
        .expect("accepté du premier coup");
    let poses = etat.set_uri.lock().await;
    assert_eq!(poses.len(), 1);
    assert_eq!(mime_annonce(&poses[0]).as_deref(), Some("audio/flac"));
    assert!(poses[0].contains("upnp:album"), "DIDL complète inchangée");
    drop(poses);
    assert_eq!(etat.get_protocol_info.load(Ordering::Relaxed), 0);
    assert_eq!(repli::profil_memorise(udn, "audio/flac"), None);
    vie.abort();
}

/// Contre-épreuve : une faute qui ne parle pas de format (701) n'appelle aucun
/// repli de profil.
#[tokio::test]
async fn une_faute_hors_format_n_appelle_aucun_repli() {
    let udn = "uuid:banc-repli-701";
    repli::oublier_profil(udn, "audio/flac");
    let etat = Strict::new(&[], &["http-get:*:audio/flac:*"], 701);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    let err = sortie
        .play_media(&piste("http://127.0.0.1:9/stream/faute-701.flac"))
        .await
        .expect_err("701 remonte");
    assert!(err.contains("701"), "{err}");
    assert_eq!(etat.set_uri.lock().await.len(), 1);
    assert_eq!(etat.get_protocol_info.load(Ordering::Relaxed), 0);
    vie.abort();
}
