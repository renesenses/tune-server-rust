//! 🔴 #6008 — un renderer qui remonte son volume à chaque nouvelle URI.
//!
//! pissa7 (fil 2187, Cabasse Abyss, firmware 25.04.10) : le volume saute vers
//! ~80 % quand il choisit une autre piste, sans qu'aucun `SetVolume` ne parte
//! de Tune. Décision de Bertrand du 12/10 : Tune réimpose le dernier volume
//! après chaque `SetAVTransportURI` sur ce renderer, par une règle de
//! compatibilité APPRISE par appareil, pas par son nom.
//!
//! Le faux renderer imite le défaut : chaque `SetAVTransportURI` accepté remet
//! son volume à 80. Son jumeau sain garde le volume qu'on lui a donné. Ce banc
//! ne prétend pas reproduire l'Abyss lui-même, dont on n'a ni le SCPD ni les
//! évènements : il prouve la conduite de Tune face à ce comportement.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use tokio::sync::Mutex;

use crate::outputs::dlna::DlnaOutput;
use crate::outputs::dlna_repli_set_uri as compat;
use crate::outputs::traits::{OutputTarget, PlayMedia};

#[derive(Clone)]
struct FauxRenderer {
    /// Remet le volume à 80 à chaque `SetAVTransportURI` accepté.
    remonte_a_l_uri: bool,
    volume: Arc<Mutex<u32>>,
    set_volume: Arc<Mutex<Vec<u32>>>,
    current_uri: Arc<Mutex<String>>,
}

impl FauxRenderer {
    fn new(remonte_a_l_uri: bool) -> Self {
        Self {
            remonte_a_l_uri,
            volume: Arc::new(Mutex::new(50)),
            set_volume: Arc::default(),
            current_uri: Arc::default(),
        }
    }
}

fn enveloppe(corps: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{corps}</s:Body></s:Envelope>"#
    )
}

fn action(corps: &str) -> String {
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

async fn av(State(s): State<FauxRenderer>, corps: String) -> axum::response::Response {
    match action(&corps).as_str() {
        "SetAVTransportURI" => {
            *s.current_uri.lock().await = tag(&corps, "CurrentURI");
            if s.remonte_a_l_uri {
                // Le défaut imité : le firmware rétablit SON volume de départ.
                *s.volume.lock().await = 80;
            }
            enveloppe("<u:SetAVTransportURIResponse/>").into_response()
        }
        "GetTransportInfo" => enveloppe(
            "<u:GetTransportInfoResponse><CurrentTransportState>PLAYING</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse>",
        )
        .into_response(),
        "GetMediaInfo" | "GetPositionInfo" => {
            let uri = s.current_uri.lock().await.clone();
            enveloppe(&format!(
                "<u:GetPositionInfoResponse><CurrentURI>{uri}</CurrentURI><TrackURI>{uri}</TrackURI><RelTime>0:00:01</RelTime><TrackDuration>0:03:00</TrackDuration></u:GetPositionInfoResponse>"
            ))
            .into_response()
        }
        autre => enveloppe(&format!("<u:{autre}Response/>")).into_response(),
    }
}

async fn rc(State(s): State<FauxRenderer>, corps: String) -> axum::response::Response {
    match action(&corps).as_str() {
        "SetVolume" => {
            let n = tag(&corps, "DesiredVolume")
                .trim()
                .parse::<u32>()
                .unwrap_or(0);
            *s.volume.lock().await = n;
            s.set_volume.lock().await.push(n);
            enveloppe("<u:SetVolumeResponse/>").into_response()
        }
        "GetVolume" => {
            let n = *s.volume.lock().await;
            enveloppe(&format!(
                "<u:GetVolumeResponse><CurrentVolume>{n}</CurrentVolume></u:GetVolumeResponse>"
            ))
            .into_response()
        }
        autre => enveloppe(&format!("<u:{autre}Response/>")).into_response(),
    }
}

async fn cm(corps: String) -> axum::response::Response {
    if action(&corps) == "GetProtocolInfo" {
        return enveloppe(
            "<u:GetProtocolInfoResponse><Source></Source><Sink>http-get:*:audio/flac:*</Sink></u:GetProtocolInfoResponse>",
        )
        .into_response();
    }
    enveloppe("<u:Response/>").into_response()
}

async fn demarrer(etat: FauxRenderer, udn: &str) -> (DlnaOutput, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/av", post(av))
        .route("/rc", post(rc))
        .route("/cm", post(cm))
        .with_state(etat);
    let vie = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let sortie = DlnaOutput::new(
        "Abyss banc".into(),
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
        title: Some("Mahler 6"),
        artist: Some("Artiste"),
        album: Some("Album"),
        duration_ms: Some(180_000),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        channels: Some(2),
        ..Default::default()
    }
}

/// ⭐ Le cas du fil 2187. Tune a posé 30 % ; la nouvelle URI fait remonter
/// l'appareil à 80 %. Le premier saut est vu au sondage qui suit : la règle
/// s'apprend, 30 % est réimposé et le sondeur ne voit jamais 80 %. À la piste
/// suivante, le volume est reposé dès la fin de la pose, sans attendre.
#[tokio::test]
async fn un_renderer_qui_remonte_son_volume_a_l_uri_se_voit_reimposer_le_dernier_volume() {
    let udn = "uuid:banc-6008-abyss";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let etat = FauxRenderer::new(true);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    assert_eq!(*etat.volume.lock().await, 30);

    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/6008-a.flac"))
        .await
        .expect("la première piste part");
    assert_eq!(
        *etat.volume.lock().await,
        80,
        "le faux renderer a bien remonté son volume à la pose (sinon le banc ne mesure rien)"
    );

    let statut = sortie.get_status().await.unwrap();
    assert!(
        (statut.volume - 0.30).abs() < 0.005,
        "le sondeur doit recevoir le volume RÉIMPOSÉ, pas le saut : {}",
        statut.volume
    );
    assert_eq!(
        *etat.volume.lock().await,
        30,
        "Tune doit avoir reposé 30 % sur l'appareil"
    );
    assert!(
        compat::volume_remonte_a_l_uri(udn),
        "la règle doit être apprise pour cet appareil"
    );
    assert!(
        compat::compatibilite_de(udn)
            .volume_reimpose_depuis
            .is_some(),
        "la règle doit se lire dans la compatibilité de l'appareil"
    );

    // Piste suivante : la règle est portée, le volume est reposé tout de suite.
    let avant = etat.set_volume.lock().await.len();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/6008-b.flac"))
        .await
        .expect("la deuxième piste part");
    assert_eq!(
        *etat.volume.lock().await,
        30,
        "après la pose d'URI, le dernier volume doit être réimposé sans attendre le sondage"
    );
    assert_eq!(
        etat.set_volume.lock().await[avant..],
        [30],
        "un seul SetVolume, au dernier volume"
    );

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Contre-épreuve : un renderer sain garde son volume. Aucune règle apprise,
/// aucun `SetVolume` en plus de celui de Tune.
#[tokio::test]
async fn un_renderer_sain_ne_recoit_aucun_volume_reimpose() {
    let udn = "uuid:banc-6008-sain";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let etat = FauxRenderer::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    for url in [
        "http://127.0.0.1:9/stream/6008-sain-a.flac",
        "http://127.0.0.1:9/stream/6008-sain-b.flac",
    ] {
        sortie.play_media(&piste(url)).await.expect("la piste part");
        let statut = sortie.get_status().await.unwrap();
        assert!((statut.volume - 0.30).abs() < 0.005);
    }
    assert_eq!(
        *etat.set_volume.lock().await,
        [30],
        "seul le SetVolume de Tune est parti"
    );
    assert!(!compat::volume_remonte_a_l_uri(udn));
    vie.abort();
}

/// Contre-épreuve : une BAISSE juste après la pose (la main de l'auditeur)
/// n'est ni apprise ni corrigée.
#[tokio::test]
async fn une_baisse_apres_la_pose_n_est_pas_corrigee() {
    let udn = "uuid:banc-6008-baisse";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let etat = FauxRenderer::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/6008-baisse.flac"))
        .await
        .expect("la piste part");
    *etat.volume.lock().await = 12;
    let statut = sortie.get_status().await.unwrap();
    assert!((statut.volume - 0.12).abs() < 0.005, "{}", statut.volume);
    assert_eq!(*etat.set_volume.lock().await, [30]);
    assert!(!compat::volume_remonte_a_l_uri(udn));
    vie.abort();
}
