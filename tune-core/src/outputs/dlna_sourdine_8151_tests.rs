//! 🔴 #8151 — une zone DLNA en sourdine repartait AVEC le son à chaque piste.
//!
//! Bertrand, 08/11 (.18, Eversolo DMP-A8) : `dlna_set_uri_ok` → `Play` →
//! `dlna_volume_reimpose_apres_uri volume_pct=80`, aucun `SetMute`. Le faux
//! renderer imite le pire cas : chaque `SetAVTransportURI` remonte son volume
//! à 80 ET lève sa sourdine. Il note, à chaque `Play`, s'il était audible —
//! c'est la seule question qui vaille : « une note est-elle sortie ? ».
//!
//! Aucun son : tout se passe en SOAP sur 127.0.0.1, sans appareil.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use tokio::sync::Mutex;

use crate::outputs::dlna::DlnaOutput;
use crate::outputs::dlna_repli_set_uri as compat;
use crate::outputs::traits::{OutputTarget, PlayMedia};

#[derive(Clone, Default)]
pub(crate) struct FauxEversolo {
    /// Refuse `SetMute` (appareil sans sourdine).
    pub(crate) sans_sourdine: bool,
    pub(crate) volume: Arc<Mutex<u32>>,
    pub(crate) muet: Arc<Mutex<bool>>,
    /// Ce que l'appareil a reçu, dans l'ordre : `SetMute(1)`, `SetVolume(30)`…
    pub(crate) commandes: Arc<Mutex<Vec<String>>>,
    /// À chaque `Play` : le volume qui sortait, 0 si muet.
    pub(crate) audible_au_play: Arc<Mutex<Vec<u32>>>,
    current_uri: Arc<Mutex<String>>,
}

impl FauxEversolo {
    pub(crate) fn new(sans_sourdine: bool) -> Self {
        Self {
            sans_sourdine,
            volume: Arc::new(Mutex::new(50)),
            ..Default::default()
        }
    }

    /// Le volume que l'auditeur entend en ce moment.
    pub(crate) async fn audible(&self) -> u32 {
        if *self.muet.lock().await {
            0
        } else {
            *self.volume.lock().await
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

const REFUS_401: &str = "<s:Fault><detail><UPnPError><errorCode>401</errorCode><errorDescription>Invalid Action</errorDescription></UPnPError></detail></s:Fault>";

async fn av(State(s): State<FauxEversolo>, corps: String) -> axum::response::Response {
    match action(&corps).as_str() {
        "SetAVTransportURI" => {
            *s.current_uri.lock().await = tag(&corps, "CurrentURI");
            // Le défaut imité : le firmware repart à SON volume, son ouvert.
            *s.volume.lock().await = 80;
            *s.muet.lock().await = false;
            enveloppe("<u:SetAVTransportURIResponse/>").into_response()
        }
        "Play" => {
            let audible = s.audible().await;
            s.audible_au_play.lock().await.push(audible);
            s.commandes.lock().await.push("Play".into());
            enveloppe("<u:PlayResponse/>").into_response()
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

async fn rc(State(s): State<FauxEversolo>, corps: String) -> axum::response::Response {
    match action(&corps).as_str() {
        "SetVolume" => {
            let n = tag(&corps, "DesiredVolume")
                .trim()
                .parse::<u32>()
                .unwrap_or(0);
            *s.volume.lock().await = n;
            s.commandes.lock().await.push(format!("SetVolume({n})"));
            enveloppe("<u:SetVolumeResponse/>").into_response()
        }
        "SetMute" => {
            let m = tag(&corps, "DesiredMute").trim().to_string();
            s.commandes.lock().await.push(format!("SetMute({m})"));
            if s.sans_sourdine {
                return enveloppe(REFUS_401).into_response();
            }
            *s.muet.lock().await = m == "1";
            enveloppe("<u:SetMuteResponse/>").into_response()
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

/// Démarre le faux appareil et une sortie DLNA NEUVE vers lui — neuve comme
/// après un redémarrage du serveur : elle ne sait rien de la sourdine.
pub(crate) async fn demarrer(
    etat: FauxEversolo,
    udn: &str,
) -> (DlnaOutput, tokio::task::JoinHandle<()>) {
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
        "Eversolo banc".into(),
        udn.into(),
        "127.0.0.1".into(),
        format!("http://127.0.0.1:{port}/av"),
        format!("http://127.0.0.1:{port}/rc"),
        Some(format!("http://127.0.0.1:{port}/cm")),
    );
    (sortie, vie)
}

pub(crate) fn piste(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        title: Some("Piste 8151"),
        artist: Some("Artiste"),
        album: Some("Album"),
        duration_ms: Some(180_000),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        channels: Some(2),
        ..Default::default()
    }
}

/// ⭐ Le cas de Bertrand. La règle « volume remonté à l'URI » est portée
/// (#6008), la sortie est en sourdine, deux pistes se suivent : aucune note
/// ne sort, à aucun `Play`, et rien ne lève la sourdine.
#[tokio::test]
async fn une_sortie_dlna_en_sourdine_reste_muette_a_chaque_nouvelle_piste() {
    let udn = "uuid:banc-8151-eversolo";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxEversolo::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    assert_eq!(etat.audible().await, 0);

    for url in [
        "http://127.0.0.1:9/stream/8151-a.flac",
        "http://127.0.0.1:9/stream/8151-podcast.mp3",
    ] {
        sortie.play_media(&piste(url)).await.expect("la piste part");
        assert_eq!(
            etat.audible().await,
            0,
            "#8151 : la zone en sourdine est repartie AVEC le son ({url}) ; reçu : {:?}",
            etat.commandes.lock().await
        );
        // Le sondeur passe : il ne doit pas non plus rendre le son.
        let _ = sortie.get_status().await.unwrap();
        assert_eq!(etat.audible().await, 0, "le sondeur a rendu le son ({url})");
    }
    assert_eq!(
        *etat.audible_au_play.lock().await,
        [0, 0],
        "chaque Play doit trouver l'appareil déjà muet"
    );
    assert!(
        !etat
            .commandes
            .lock()
            .await
            .iter()
            .any(|c| c == "SetMute(0)"),
        "aucune commande ne doit lever la sourdine : {:?}",
        etat.commandes.lock().await
    );

    // L'auditeur lève la sourdine : le volume qu'il avait est rendu, pas les
    // 80 % du firmware.
    sortie.set_mute(false).await.unwrap();
    assert_eq!(
        etat.audible().await,
        30,
        "sourdine levée : le volume doit être rendu à 30 % ; reçu : {:?}",
        etat.commandes.lock().await
    );

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Même cas sur un appareil SANS la règle #6008 : un firmware qui lève sa
/// sourdine à l'URI ne fait pas sortir le son pour autant.
#[tokio::test]
async fn une_sortie_dlna_sans_regle_de_volume_garde_aussi_sa_sourdine() {
    let udn = "uuid:banc-8151-sans-regle";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let etat = FauxEversolo::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_mute(true).await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/8151-sans-regle.flac"))
        .await
        .expect("la piste part");
    assert_eq!(
        *etat.audible_au_play.lock().await,
        [0],
        "le Play doit trouver l'appareil muet ; reçu : {:?}",
        etat.commandes.lock().await
    );
    assert_eq!(etat.audible().await, 0);

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Un appareil qui refuse `SetMute` : la sourdine tient par un volume nul, la
/// consigne d'avant est gardée et rendue à la levée.
#[tokio::test]
async fn un_appareil_sans_sourdine_tient_la_sourdine_par_un_volume_nul() {
    let udn = "uuid:banc-8151-sans-sourdine";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxEversolo::new(true);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    assert_eq!(*etat.volume.lock().await, 0, "repli : volume nul");

    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/8151-sans-sourdine.flac"))
        .await
        .expect("la piste part");
    assert_eq!(
        etat.audible().await,
        0,
        "le volume nul doit être reposé après l'URI ; reçu : {:?}",
        etat.commandes.lock().await
    );
    assert_eq!(*etat.audible_au_play.lock().await, [0]);
    let _ = sortie.get_status().await.unwrap();
    assert_eq!(etat.audible().await, 0, "le sondeur a rendu le son");

    sortie.set_mute(false).await.unwrap();
    assert_eq!(etat.audible().await, 30, "la consigne d'avant est rendue");

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Contre-épreuve : hors sourdine, la réimposition #6008 est inchangée — un
/// seul `SetVolume` au dernier volume, aucun `SetMute`.
#[tokio::test]
async fn hors_sourdine_la_reimposition_du_volume_est_inchangee() {
    let udn = "uuid:banc-8151-hors-sourdine";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxEversolo::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    etat.commandes.lock().await.clear();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/8151-hors.flac"))
        .await
        .expect("la piste part");
    assert_eq!(*etat.commandes.lock().await, ["Play", "SetVolume(30)"]);
    assert_eq!(etat.audible().await, 30);

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}
