//! #2991 — épreuves de la réémission du titre radio par DIDL, contre un faux
//! renderer DLNA qui enregistre chaque action SOAP et son corps.
//!
//! Le faux suit le minimum du contrat AVTransport dont `play_media` a besoin :
//! `GetMediaInfo` rend l'URI posée, `GetTransportInfo` dit `STOPPED` avant le
//! `Play` et `PLAYING` après.
use super::*;
use crate::outputs::traits::OutputTarget;
use axum::{Router, http::StatusCode, routing::post};
use std::sync::Mutex;

/// Une action reçue : son nom (`SetNextAVTransportURI`…) et son corps brut.
#[derive(Debug, Clone)]
pub(crate) struct Recue {
    pub(crate) action: String,
    pub(crate) corps: String,
}

#[derive(Default)]
struct Etat {
    uri: String,
    en_lecture: bool,
    /// `SetAVTransportURI` remet le transport à l'arrêt (ce que font la
    /// plupart des renderers) : sert à l'épreuve du mode « recharger ».
    arret_sur_set_uri: bool,
}

pub(crate) struct Serveur(tokio::task::JoinHandle<()>);

impl Drop for Serveur {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct FauxRenderer {
    pub(crate) recues: Arc<Mutex<Vec<Recue>>>,
    pub(crate) host: String,
    pub(crate) _serveur: Serveur,
}

impl FauxRenderer {
    pub(crate) fn sortie(&self, device_id: &str) -> DlnaOutput {
        DlnaOutput::new(
            "Rose test".into(),
            device_id.into(),
            self.host.clone(),
            format!("{}/control", self.host),
            format!("{}/control", self.host),
            None,
        )
    }

    pub(crate) fn actions(&self, action: &str) -> Vec<Recue> {
        actions_dans(&self.recues, action)
    }

    pub(crate) fn total(&self) -> usize {
        self.recues.lock().unwrap().len()
    }
}

pub(crate) fn actions_dans(recues: &Mutex<Vec<Recue>>, action: &str) -> Vec<Recue> {
    recues
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.action == action)
        .cloned()
        .collect()
}

fn entre<'a>(corps: &'a str, balise: &str) -> &'a str {
    let ouvrante = format!("<{balise}>");
    let fermante = format!("</{balise}>");
    corps
        .split_once(&ouvrante)
        .and_then(|(_, reste)| reste.split_once(&fermante))
        .map(|(v, _)| v)
        .unwrap_or("")
}

pub(crate) async fn faux_renderer(arret_sur_set_uri: bool) -> FauxRenderer {
    let recues = Arc::new(Mutex::new(Vec::<Recue>::new()));
    let etat = Arc::new(Mutex::new(Etat {
        arret_sur_set_uri,
        ..Default::default()
    }));
    let (r, e) = (recues.clone(), etat.clone());
    let app = Router::new().route(
        "/control",
        post(move |headers: axum::http::HeaderMap, corps: String| {
            let (recues, etat) = (r.clone(), e.clone());
            async move {
                let soap = headers
                    .get("SOAPAction")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let action = soap
                    .trim_matches('"')
                    .rsplit('#')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                recues.lock().unwrap().push(Recue {
                    action: action.clone(),
                    corps: corps.clone(),
                });
                let mut etat = etat.lock().unwrap();
                let reponse = match action.as_str() {
                    "SetAVTransportURI" => {
                        etat.uri = entre(&corps, "CurrentURI").to_string();
                        if etat.arret_sur_set_uri {
                            etat.en_lecture = false;
                        }
                        "<u:SetAVTransportURIResponse/>".to_string()
                    }
                    "Play" => {
                        etat.en_lecture = true;
                        "<u:PlayResponse/>".to_string()
                    }
                    "Stop" => {
                        etat.en_lecture = false;
                        "<u:StopResponse/>".to_string()
                    }
                    "GetTransportInfo" => format!(
                        "<s:Envelope><s:Body><u:GetTransportInfoResponse><CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse></s:Body></s:Envelope>",
                        if etat.en_lecture { "PLAYING" } else { "STOPPED" }
                    ),
                    "GetMediaInfo" => format!(
                        "<s:Envelope><s:Body><u:GetMediaInfoResponse><NrTracks>1</NrTracks><CurrentURI>{}</CurrentURI></u:GetMediaInfoResponse></s:Body></s:Envelope>",
                        etat.uri
                    ),
                    _ => "<Response/>".to_string(),
                };
                (StatusCode::OK, reponse)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FauxRenderer {
        recues,
        host,
        _serveur: Serveur(task),
    }
}

pub(crate) const URL_RADIO: &str = "http://127.0.0.1:9/stream/sid-2991.wav";

/// La pose d'une radio, telle que l'orchestrateur la confie à la sortie.
pub(crate) async fn poser_la_radio(sortie: &DlnaOutput) {
    sortie
        .play_media(&PlayMedia {
            url: URL_RADIO,
            mime_type: "audio/wav",
            title: Some("RP Mellow"),
            album: Some("RP Mellow"),
            live_stream: true,
            source: Some("radio"),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            ..Default::default()
        })
        .await
        .expect("la pose de la radio doit aboutir sur le faux renderer");
}

pub(crate) fn titre_lyle() -> TitreRadio {
    TitreRadio {
        titre: "Friend Of The Devil".into(),
        artiste: Some("Lyle Lovett".into()),
        album: Some("RP Mellow".into()),
        pochette: Some("https://img.radioparadise.com/covers/l/9039.jpg".into()),
    }
}

#[test]
fn le_reglage_se_relit_et_tout_l_inconnu_vaut_desactive() {
    assert_eq!(
        ModeTitreRadioDidl::depuis_reglage("next"),
        ModeTitreRadioDidl::Suivante
    );
    assert_eq!(
        ModeTitreRadioDidl::depuis_reglage("reload"),
        ModeTitreRadioDidl::Recharger
    );
    assert_eq!(
        ModeTitreRadioDidl::depuis_reglage("off"),
        ModeTitreRadioDidl::Desactive
    );
    assert_eq!(
        ModeTitreRadioDidl::depuis_reglage("true"),
        ModeTitreRadioDidl::Desactive
    );
    assert_eq!(ModeTitreRadioDidl::depuis_api("n'importe"), None);
    assert_eq!(
        ModeTitreRadioDidl::depuis_api(""),
        Some(ModeTitreRadioDidl::Desactive)
    );
    assert_eq!(cle_de_zone(8), "zone_8_dlna_radio_title");
}

/// La pose d'URI acceptée est retenue : sans elle, rien ne saurait quelle URI
/// rejouer ni sous quel MIME.
#[tokio::test]
async fn la_pose_acceptee_est_retenue() {
    let r = faux_renderer(false).await;
    let sortie = r.sortie("uuid:2991-pose");
    poser_la_radio(&sortie).await;
    let pose = sortie
        .pose_courante()
        .expect("play_media doit retenir la pose acceptée");
    assert_eq!(pose.url, URL_RADIO);
    assert!(pose.live_stream);
    assert_eq!(pose.niveau_didl, 0);
}

/// Voie « next » : UN `SetNextAVTransportURI` sur la MÊME URI, dont le DIDL
/// porte le titre, l'artiste et la pochette ; aucun `SetAVTransportURI` ni
/// `Stop` de plus — le flux en cours n'est pas touché.
#[tokio::test]
async fn la_voie_next_envoie_le_didl_du_morceau_sans_toucher_au_flux() {
    let r = faux_renderer(false).await;
    let sortie = r.sortie("uuid:2991-next");
    poser_la_radio(&sortie).await;
    let set_uri_avant = r.actions("SetAVTransportURI").len();
    let stop_avant = r.actions("Stop").len();

    let issue = sortie
        .reemettre_titre_radio(ModeTitreRadioDidl::Suivante, URL_RADIO, &titre_lyle())
        .await
        .unwrap();
    assert_eq!(
        issue,
        IssueRelai::Envoye {
            action: "SetNextAVTransportURI"
        }
    );
    let next = r.actions("SetNextAVTransportURI");
    assert_eq!(next.len(), 1, "{next:?}");
    let corps = &next[0].corps;
    assert_eq!(entre(corps, "NextURI"), URL_RADIO);
    let didl = entre(corps, "NextURIMetaData");
    assert!(
        didl.contains("Friend Of The Devil"),
        "titre absent du DIDL : {didl}"
    );
    assert!(
        didl.contains("Lyle Lovett"),
        "artiste absent du DIDL : {didl}"
    );
    assert!(
        didl.contains("https://img.radioparadise.com/covers/l/9039.jpg"),
        "pochette absente du DIDL : {didl}"
    );
    assert_eq!(r.actions("SetAVTransportURI").len(), set_uri_avant);
    assert_eq!(r.actions("Stop").len(), stop_avant);
}

/// Voie « reload » : `SetAVTransportURI` sur la même URI avec le nouveau DIDL,
/// puis `Play` parce que le renderer s'est arrêté.
#[tokio::test]
async fn la_voie_reload_repose_la_meme_uri_et_relance_si_l_appareil_s_arrete() {
    let r = faux_renderer(true).await;
    let sortie = r.sortie("uuid:2991-reload");
    poser_la_radio(&sortie).await;
    let play_avant = r.actions("Play").len();

    let issue = sortie
        .reemettre_titre_radio(ModeTitreRadioDidl::Recharger, URL_RADIO, &titre_lyle())
        .await
        .unwrap();
    assert_eq!(
        issue,
        IssueRelai::Envoye {
            action: "SetAVTransportURI"
        }
    );
    let poses = r.actions("SetAVTransportURI");
    let derniere = &poses.last().unwrap().corps;
    assert_eq!(entre(derniere, "CurrentURI"), URL_RADIO);
    let didl = entre(derniere, "CurrentURIMetaData");
    assert!(didl.contains("Friend Of The Devil"), "{didl}");
    assert!(
        didl.contains("https://img.radioparadise.com/covers/l/9039.jpg"),
        "{didl}"
    );
    assert_eq!(
        r.actions("Play").len(),
        play_avant + 1,
        "Play attendu après l'arrêt"
    );
}

/// Réglage désactivé : aucune action SOAP.
#[tokio::test]
async fn desactive_rien_ne_part() {
    let r = faux_renderer(false).await;
    let sortie = r.sortie("uuid:2991-off");
    poser_la_radio(&sortie).await;
    let total = r.total();
    let issue = sortie
        .reemettre_titre_radio(ModeTitreRadioDidl::Desactive, URL_RADIO, &titre_lyle())
        .await
        .unwrap();
    assert_eq!(issue, IssueRelai::Ignore("reglage_desactive"));
    assert_eq!(r.total(), total);
}

/// Une autre URI posée entre-temps (piste de file) n'est jamais remplacée.
#[tokio::test]
async fn une_autre_uri_posee_n_est_pas_remplacee() {
    let r = faux_renderer(false).await;
    let sortie = r.sortie("uuid:2991-autre");
    poser_la_radio(&sortie).await;
    let total = r.total();
    let issue = sortie
        .reemettre_titre_radio(
            ModeTitreRadioDidl::Suivante,
            "http://127.0.0.1:9/stream/autre.flac",
            &titre_lyle(),
        )
        .await
        .unwrap();
    assert_eq!(issue, IssueRelai::Ignore("uri_posee_differente"));
    assert_eq!(r.total(), total);
}
