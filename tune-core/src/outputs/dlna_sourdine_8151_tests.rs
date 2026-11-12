//! 🔴 #8151 — une zone DLNA en sourdine repartait AVEC le son à chaque piste.
//!
//! Bertrand, 08/11 (.18, Eversolo DMP-A8) : `dlna_set_uri_ok` → `Play` →
//! `dlna_volume_reimpose_apres_uri volume_pct=80`, aucun `SetMute`. Le faux
//! renderer imite le pire cas : chaque `SetAVTransportURI` remonte son volume
//! à 80 ET lève sa sourdine. Il note, à chaque `Play` et après chaque
//! commande reçue, s'il était audible — c'est la seule question qui vaille :
//! « une note est-elle sortie ? ». Une variante lève aussi sa sourdine sur
//! `SetVolume`, l'autre cas que le diagnostic laissait ouvert.
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
    /// Lève sa sourdine sur `SetVolume`, comme certains firmwares.
    pub(crate) set_volume_leve_la_sourdine: bool,
    pub(crate) volume: Arc<Mutex<u32>>,
    pub(crate) muet: Arc<Mutex<bool>>,
    /// Ce que l'appareil a reçu, dans l'ordre : `SetMute(1)`, `SetVolume(30)`…
    pub(crate) commandes: Arc<Mutex<Vec<String>>>,
    /// À chaque `Play` : le volume qui sortait, 0 si muet.
    pub(crate) audible_au_play: Arc<Mutex<Vec<u32>>>,
    /// Après chaque commande reçue : la commande et ce qui sortait des
    /// haut-parleurs (0 si muet ou à l'arrêt).
    pub(crate) son_apres_commande: Arc<Mutex<Vec<(String, u32)>>>,
    en_lecture: Arc<Mutex<bool>>,
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

    /// Un appareil qui lève aussi sa sourdine sur `SetVolume`.
    pub(crate) fn qui_leve_la_sourdine_sur_set_volume() -> Self {
        Self {
            set_volume_leve_la_sourdine: true,
            ..Self::new(false)
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

    /// Ce qui sort des haut-parleurs : rien à l'arrêt, sinon [`Self::audible`].
    async fn son(&self) -> u32 {
        if *self.en_lecture.lock().await {
            self.audible().await
        } else {
            0
        }
    }

    async fn noter(&self, commande: String) {
        let son = self.son().await;
        self.son_apres_commande.lock().await.push((commande, son));
    }

    /// Les commandes après lesquelles une note est sortie.
    pub(crate) async fn commandes_audibles(&self) -> Vec<(String, u32)> {
        self.son_apres_commande
            .lock()
            .await
            .iter()
            .filter(|(_, son)| *son > 0)
            .cloned()
            .collect()
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
    let nom = action(&corps);
    let reponse = av_interne(&s, &corps, &nom).await;
    s.noter(nom).await;
    reponse
}

async fn av_interne(s: &FauxEversolo, corps: &str, nom: &str) -> axum::response::Response {
    match nom {
        "SetAVTransportURI" => {
            *s.current_uri.lock().await = tag(corps, "CurrentURI");
            // Le défaut imité : le firmware s'arrête et repart à SON volume,
            // son ouvert.
            *s.en_lecture.lock().await = false;
            *s.volume.lock().await = 80;
            *s.muet.lock().await = false;
            enveloppe("<u:SetAVTransportURIResponse/>").into_response()
        }
        "Play" => {
            *s.en_lecture.lock().await = true;
            let audible = s.audible().await;
            s.audible_au_play.lock().await.push(audible);
            s.commandes.lock().await.push("Play".into());
            enveloppe("<u:PlayResponse/>").into_response()
        }
        "Stop" => {
            *s.en_lecture.lock().await = false;
            enveloppe("<u:StopResponse/>").into_response()
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
    let reponse = rc_interne(&s, &corps).await;
    let derniere = s.commandes.lock().await.last().cloned();
    // Le journal porte la commande détaillée (`SetMute(1)`…) quand elle y est.
    let nom = match derniere {
        Some(c) if c.starts_with(&action(&corps)) => c,
        _ => action(&corps),
    };
    s.noter(nom).await;
    reponse
}

async fn rc_interne(s: &FauxEversolo, corps: &str) -> axum::response::Response {
    match action(corps).as_str() {
        "SetVolume" => {
            let n = tag(corps, "DesiredVolume")
                .trim()
                .parse::<u32>()
                .unwrap_or(0);
            *s.volume.lock().await = n;
            if s.set_volume_leve_la_sourdine {
                *s.muet.lock().await = false;
            }
            s.commandes.lock().await.push(format!("SetVolume({n})"));
            enveloppe("<u:SetVolumeResponse/>").into_response()
        }
        "SetMute" => {
            let m = tag(corps, "DesiredMute").trim().to_string();
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
    assert_eq!(
        etat.commandes_audibles().await,
        Vec::<(String, u32)>::new(),
        "en sourdine, aucune commande ne doit laisser l'appareil audible"
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

/// Renvoi de relecture : un firmware qui lève AUSSI sa sourdine sur
/// `SetVolume`. Le sondeur voit le saut à 80 % après l'URI : en sourdine, il ne
/// doit reposer que la sourdine — un `SetVolume(30)` d'abord ferait sortir la
/// consigne le temps d'un aller-retour. Le faux appareil note ce qui sort
/// après CHAQUE commande : aucune ne doit le laisser audible tant que la
/// sourdine tient.
#[tokio::test]
async fn un_appareil_qui_leve_la_sourdine_sur_set_volume_ne_joue_jamais_en_sourdine() {
    let udn = "uuid:banc-8151-set-volume-leve";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxEversolo::qui_leve_la_sourdine_sur_set_volume();
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    etat.son_apres_commande.lock().await.clear();

    for url in [
        "http://127.0.0.1:9/stream/8151-sv-a.flac",
        "http://127.0.0.1:9/stream/8151-sv-b.flac",
    ] {
        sortie.play_media(&piste(url)).await.expect("la piste part");
        let _ = sortie.get_status().await.unwrap();
        let _ = sortie.get_status().await.unwrap();
    }
    let journal = etat.son_apres_commande.lock().await.clone();
    assert!(
        journal.iter().any(|(c, _)| c == "Play") && journal.iter().any(|(c, _)| c == "GetVolume"),
        "le banc doit voir les Play et le relevé du volume : {journal:?}"
    );
    assert_eq!(
        etat.commandes_audibles().await,
        Vec::<(String, u32)>::new(),
        "#8151 : en sourdine, une commande a laissé l'appareil audible ; journal : {journal:?}"
    );
    assert!(
        !etat
            .commandes
            .lock()
            .await
            .iter()
            .skip_while(|c| *c != "Play")
            .any(|c| c.starts_with("SetVolume") || c == "SetMute(0)"),
        "en sourdine, ni volume ni levée après le premier Play : {:?}",
        etat.commandes.lock().await
    );

    // La levée rend la consigne, pas les 80 % du firmware.
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

/// 🔴 #8328 — en sourdine, le saut connu de l'Abyss hors de la fenêtre ne
/// rend jamais le son. Appareil sans `SetMute` (sourdine tenue par un volume
/// nul) : le saut à 80 est vu et retenu dans la fenêtre d'une piste lue avec
/// le son ; la sourdine est posée ; hors de la fenêtre, l'appareil saute de
/// nouveau à 80. Seul le volume nul est reposé, jamais la consigne de 30.
#[tokio::test]
async fn en_sourdine_le_saut_connu_hors_fenetre_ne_rend_pas_le_son_8328() {
    let udn = "uuid:banc-8328-sourdine-hors-fenetre";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxEversolo::new(true);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(0.30).await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/8328-sourdine.flac"))
        .await
        .expect("la piste part");
    // Dans la fenêtre : l'appareil remonte à 80, Tune le ramène à 30.
    *etat.volume.lock().await = 80;
    let _ = sortie.get_status().await.unwrap();
    assert_eq!(
        etat.audible().await,
        30,
        "prémisse : saut corrigé dans la fenêtre"
    );

    sortie.set_mute(true).await.unwrap();
    assert_eq!(
        etat.audible().await,
        0,
        "prémisse : sourdine par volume nul"
    );
    sortie.vieillir_la_pose_d_uri();
    sortie.vieillir_la_demande_de_volume();
    etat.son_apres_commande.lock().await.clear();
    etat.commandes.lock().await.clear();

    // Hors de la fenêtre, l'appareil refait son saut.
    *etat.volume.lock().await = 80;
    let statut = sortie.get_status().await.unwrap();
    let commandes = etat.commandes.lock().await.clone();
    let audible = etat.audible().await;
    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();

    assert_eq!(
        audible, 0,
        "#8328 : en sourdine, l'appareil est resté audible ; commandes : {commandes:?}"
    );
    assert!(
        !commandes
            .iter()
            .any(|c| c.starts_with("SetVolume(") && c != "SetVolume(0)"),
        "#8328 : en sourdine, aucun SetVolume autre que 0 : {commandes:?}"
    );
    assert!(
        statut.volume < 0.005,
        "le sondeur reçoit 0, pas {}",
        statut.volume
    );
}
