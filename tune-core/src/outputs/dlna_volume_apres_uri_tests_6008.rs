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

/// Appareil qui porte DÉJÀ la règle « volume remonté à l'URI », comme ceux de
/// Reivax66 (fil 2307) et de pissa7 (fil 2315) en rc6 : Tune pose `consigne`,
/// une première piste part, puis l'appareil rapporte de lui-même `lu` HORS de
/// la fenêtre qui suit la pose. Rend le volume de l'appareil après la pose de
/// la piste suivante.
async fn volume_apres_la_piste_suivante(udn: &str, consigne: f64, lu: u32) -> (u32, f64) {
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxRenderer::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie.set_volume(consigne).await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/6472-a.flac"))
        .await
        .expect("la première piste part");
    sortie.vieillir_la_pose_d_uri();
    // L'appareil rapporte un volume que Tune n'a pas posé, hors fenêtre.
    *etat.volume.lock().await = lu;
    let statut = sortie.get_status().await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/6472-b.flac"))
        .await
        .expect("la piste suivante part");
    let apres = *etat.volume.lock().await;

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
    (apres, statut.volume)
}

/// 🔴 #6472 (Reivax66, fil 2307, rc6) : au passage de la radio FIP à un album,
/// Tune a mis l'ampli à 0. Le renderer avait rapporté 0 hors de la fenêtre ;
/// cette LECTURE devenait le volume connu et la pose suivante la réimposait
/// (`dlna_volume_reimpose_apres_uri … volume_pct=0`). Seule la dernière
/// consigne de Tune (52 %) peut être réimposée.
#[tokio::test]
async fn une_lecture_a_zero_n_est_jamais_reimposee_apres_l_uri() {
    let (apres, lu) = volume_apres_la_piste_suivante("uuid:banc-6472-zero", 0.52, 0).await;
    assert!(lu.abs() < 0.005, "prémisse : le sondeur a bien lu 0 ({lu})");
    assert_eq!(
        apres, 52,
        "après la pose d'URI, Tune doit reposer SA consigne (52 %), jamais un 0 lu sur l'appareil"
    );
}

/// 🔴 #6008 (pissa7, fil 2315, rc6) : même mécanisme, dans l'autre sens. La
/// Cabasse Abyss rapporte 80 % entre deux pistes, et Tune réimposait 80 %
/// alors que l'auditeur avait ramené le volume à 13 % depuis Tune.
#[tokio::test]
async fn un_80_remonte_par_l_appareil_n_est_jamais_reimpose_apres_l_uri() {
    let (apres, lu) = volume_apres_la_piste_suivante("uuid:banc-6472-abyss", 0.13, 80).await;
    assert!(
        (lu - 0.80).abs() < 0.005,
        "prémisse : le sondeur a bien lu 80 ({lu})"
    );
    assert_eq!(
        apres, 13,
        "après la pose d'URI, Tune doit reposer SA consigne (13 %), pas les 80 % lus sur l'appareil"
    );
}

/// 🔴 #6472 — zone sensible (volume). Appareil qui porte la règle « volume
/// remonté à l'URI », mais auquel Tune n'a encore donné AUCUNE consigne : il
/// rapporte 0 hors de la fenêtre (la radio de Reivax66, fil 2307), puis deux
/// changements de piste. Tune n'a rien à reposer : aucun `SetVolume` ne doit
/// partir, ni 0 ni autre chose.
#[tokio::test]
async fn sans_consigne_de_tune_aucun_set_volume_au_changement_de_piste() {
    let udn = "uuid:banc-6472-sans-consigne";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let etat = FauxRenderer::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;

    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/6472-radio.flac"))
        .await
        .expect("la radio part");
    sortie.vieillir_la_pose_d_uri();
    *etat.volume.lock().await = 0;
    let statut = sortie.get_status().await.unwrap();
    assert!(
        statut.volume.abs() < 0.005,
        "prémisse : le sondeur a bien lu 0 ({})",
        statut.volume
    );
    for url in [
        "http://127.0.0.1:9/stream/6472-album-1.flac",
        "http://127.0.0.1:9/stream/6472-album-2.flac",
    ] {
        sortie.play_media(&piste(url)).await.expect("la piste part");
    }

    let envoyes = etat.set_volume.lock().await.clone();
    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
    assert!(
        envoyes.is_empty(),
        "#6472 : aucun SetVolume ne doit partir au changement de piste quand Tune n'a posé aucune consigne ; envoyés : {envoyes:?}"
    );
}

/// Appareil qui a APPRIS la règle à la première piste, comme l'Abyss de
/// pissa7 : Tune pose `consigne`, la piste part (l'appareil remonte à 80 à la
/// pose, Tune réimpose), puis la fenêtre d'après pose et la grâce d'après
/// demande sont écoulées. Rend la sortie, son faux renderer et sa tâche.
async fn abyss_hors_fenetre(
    udn: &str,
    consigne: f64,
) -> (DlnaOutput, FauxRenderer, tokio::task::JoinHandle<()>) {
    compat::reinitialiser_compatibilite(udn).unwrap();
    let etat = FauxRenderer::new(true);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(consigne).await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/2401-a.flac"))
        .await
        .expect("la première piste part");
    // Dans la fenêtre : le saut de la pose est vu, la règle s'apprend.
    let statut = sortie.get_status().await.unwrap();
    assert!(
        (statut.volume - consigne).abs() < 0.005,
        "prémisse : le saut de la pose est corrigé dans la fenêtre ({})",
        statut.volume
    );
    assert!(
        compat::volume_remonte_a_l_uri(udn),
        "prémisse : règle apprise"
    );
    sortie.vieillir_la_pose_d_uri();
    sortie.vieillir_la_demande_de_volume();
    (sortie, etat, vie)
}

/// 🔴 #6008 (pissa7, fil 2401, rc7) — la séquence du journal. Consigne 22 %,
/// piste lancée, saut à 80 corrigé dans les 15 s ; puis, 33 s après le début
/// de la piste et sans aucun `SetVolume` de Tune, l'Abyss rapporte de nouveau
/// 80 (`volume_adopte_du_renderer ancien=22.0 nouveau=80.0`). Hors de la
/// fenêtre, Tune laissait l'appareil à 80. Désormais il le ramène à 22, le
/// sondeur reçoit 22, et aucun ordre au-dessus de 22 ne part jamais.
#[tokio::test]
async fn hors_fenetre_un_80_de_l_abyss_est_ramene_a_la_consigne() {
    let udn = "uuid:banc-6008-2401-hors-fenetre";
    let (sortie, etat, vie) = abyss_hors_fenetre(udn, 0.22).await;

    // L'appareil remonte seul, hors de la fenêtre.
    *etat.volume.lock().await = 80;
    let statut = sortie.get_status().await.unwrap();
    let tenu = *etat.volume.lock().await;
    let envoyes = etat.set_volume.lock().await.clone();
    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();

    assert_eq!(
        tenu, 22,
        "#6008 (fil 2401) : l'Abyss a remonté son volume à 80 % hors de la fenêtre d'après pose, \
         et Tune l'y a laissé ; il doit le ramener à la consigne de l'auditeur (22 %)"
    );
    assert!(
        (statut.volume - 0.22).abs() < 0.005,
        "#6008 : le sondeur doit recevoir la consigne réimposée, pas les 80 % ({})",
        statut.volume
    );
    assert!(
        envoyes.iter().all(|&v| v <= 22),
        "#6008 : aucun SetVolume au-dessus de la consigne de l'auditeur (22) ; envoyés : {envoyes:?}"
    );
}

/// Contre-épreuve : la piste en cours n'est pas celle de Tune (source propre de
/// l'appareil, autre serveur). Son volume n'est jamais touché.
#[tokio::test]
async fn hors_fenetre_une_source_qui_n_est_pas_tune_n_est_pas_touchee() {
    let udn = "uuid:banc-6008-2401-autre-source";
    let (sortie, etat, vie) = abyss_hors_fenetre(udn, 0.22).await;
    let avant = etat.set_volume.lock().await.len();

    *etat.current_uri.lock().await = "http://192.0.2.50:9000/radio-integree".into();
    *etat.volume.lock().await = 80;
    let statut = sortie.get_status().await.unwrap();
    let apres = etat.set_volume.lock().await[avant..].to_vec();
    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();

    assert!((statut.volume - 0.80).abs() < 0.005, "{}", statut.volume);
    assert!(apres.is_empty(), "aucun SetVolume : {apres:?}");
}

/// Contre-épreuve : juste après une demande de volume de Tune (glissement de
/// curseur), une lecture plus haute n'est pas jugée hors fenêtre.
#[tokio::test]
async fn hors_fenetre_rien_n_est_corrige_dans_la_grace_d_une_demande() {
    let udn = "uuid:banc-6008-2401-grace";
    let (sortie, etat, vie) = abyss_hors_fenetre(udn, 0.22).await;
    sortie.set_volume(0.22).await.unwrap();
    let avant = etat.set_volume.lock().await.len();

    *etat.volume.lock().await = 40;
    let statut = sortie.get_status().await.unwrap();
    let apres = etat.set_volume.lock().await[avant..].to_vec();
    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();

    assert!((statut.volume - 0.40).abs() < 0.005, "{}", statut.volume);
    assert!(
        apres.is_empty(),
        "aucun SetVolume dans la grâce : {apres:?}"
    );
}

/// Contre-épreuve : une BAISSE hors fenêtre (la main de l'auditeur) n'est
/// jamais corrigée.
#[tokio::test]
async fn hors_fenetre_une_baisse_n_est_pas_corrigee() {
    let udn = "uuid:banc-6008-2401-baisse";
    let (sortie, etat, vie) = abyss_hors_fenetre(udn, 0.22).await;
    let avant = etat.set_volume.lock().await.len();

    *etat.volume.lock().await = 10;
    let statut = sortie.get_status().await.unwrap();
    let apres = etat.set_volume.lock().await[avant..].to_vec();
    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();

    assert!((statut.volume - 0.10).abs() < 0.005, "{}", statut.volume);
    assert!(apres.is_empty(), "aucun SetVolume : {apres:?}");
}
