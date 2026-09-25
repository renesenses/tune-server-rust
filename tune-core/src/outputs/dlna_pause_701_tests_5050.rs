//! #5050 — une `Pause` refusée en 701 « Transition not available » pendant
//! que le renderer se repositionne n'est pas un échec définitif.
//!
//! FabienM (Beosound Stage, zone Parents, 0.9.165, fil 1943) : six Pause
//! refusées en 701 de 3 à 51 s après un `Seek`, et `GetTransportInfo` qui
//! rend `TRANSITIONING` 16 ms après le Seek de reprise. `pause()` rendait le
//! premier refus à l'interface sans relire l'état ni réessayer.
//!
//! Le renderer factice suit le contrat UPnP : il est en `TRANSITIONING`
//! pendant une durée donnée, refuse la pause en 701 tant qu'il y est, puis
//! passe en `PLAYING` et accepte la transition PLAYING→PAUSED_PLAYBACK.
//!
//! Contre-épreuve : remettre dans `pause()` le seul
//! `acquitter_commande_soap("Pause", response)` fait tomber
//! `une_pause_refusee_pendant_la_transition_aboutit_quand_le_renderer_en_sort`
//! et `un_renderer_deja_en_pause_n_est_pas_un_echec`.
//!
//! Le même factice sert à l'orchestrateur (#5050, Seek de reprise
//! conditionnel) : il répond à `GetPositionInfo` avec le `RelTime` posé dans
//! [`Etat::rel_time`] — ou sans `RelTime` du tout, une position illisible.
use super::*;
use axum::{Router, http::StatusCode, routing::post};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FAUTE_701: &str = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

/// Ce que le renderer factice déclare, et quand.
#[derive(Clone, Copy)]
pub(crate) enum Scenario {
    /// `TRANSITIONING` pendant cette durée, puis `PLAYING`.
    TransitionPuisLecture(Duration),
    /// Toujours cet état ; la pause y est toujours refusée en 701.
    Fige(&'static str),
}

pub(crate) struct Etat {
    debut: Instant,
    scenario: Scenario,
    pub(crate) en_pause: bool,
    /// Ce que `GetPositionInfo` rend dans `RelTime` ; `None` : la réponse
    /// n'en porte pas (position illisible).
    pub(crate) rel_time: Option<&'static str>,
}

impl Etat {
    pub(crate) fn courant(&self) -> &'static str {
        if self.en_pause {
            return "PAUSED_PLAYBACK";
        }
        match self.scenario {
            Scenario::TransitionPuisLecture(d) if self.debut.elapsed() < d => "TRANSITIONING",
            Scenario::TransitionPuisLecture(_) => "PLAYING",
            Scenario::Fige(e) => e,
        }
    }
}

/// Le serveur factice s'arrête avec ce gardien, pas avec le `Renderer` :
/// on peut ainsi céder la sortie à un registre (`Renderer` se déstructure)
/// sans couper le serveur.
pub(crate) struct Serveur(tokio::task::JoinHandle<()>);

impl Drop for Serveur {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct Renderer {
    pub(crate) output: DlnaOutput,
    pub(crate) recues: Arc<Mutex<Vec<String>>>,
    pub(crate) etat: Arc<Mutex<Etat>>,
    pub(crate) task: Serveur,
}

/// Combien de fois `action` a été reçue.
pub(crate) fn compte_dans(recues: &Mutex<Vec<String>>, action: &str) -> usize {
    recues
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.ends_with(&format!("#{action}\"")))
        .count()
}

impl Renderer {
    fn compte(&self, action: &str) -> usize {
        compte_dans(&self.recues, action)
    }
}

pub(crate) async fn renderer(scenario: Scenario) -> Renderer {
    let recues = Arc::new(Mutex::new(Vec::new()));
    let etat = Arc::new(Mutex::new(Etat {
        debut: Instant::now(),
        scenario,
        en_pause: false,
        rel_time: None,
    }));
    let (r, e) = (recues.clone(), etat.clone());
    let app = Router::new().route(
        "/control",
        post(move |headers: axum::http::HeaderMap, _body: String| {
            let (recues, etat) = (r.clone(), e.clone());
            async move {
                let action = headers
                    .get("SOAPAction")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                recues.lock().unwrap().push(action.clone());
                let mut etat = etat.lock().unwrap();
                if action.ends_with("#GetTransportInfo\"") {
                    let corps = format!(
                        "<s:Envelope><s:Body><u:GetTransportInfoResponse><CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse></s:Body></s:Envelope>",
                        etat.courant()
                    );
                    (StatusCode::OK, corps)
                } else if action.ends_with("#GetPositionInfo\"") {
                    let rel = etat
                        .rel_time
                        .map(|t| format!("<RelTime>{t}</RelTime>"))
                        .unwrap_or_default();
                    let corps = format!(
                        "<s:Envelope><s:Body><u:GetPositionInfoResponse><Track>1</Track><TrackDuration>0:05:00</TrackDuration>{rel}</u:GetPositionInfoResponse></s:Body></s:Envelope>"
                    );
                    (StatusCode::OK, corps)
                } else if action.ends_with("#Pause\"") {
                    if etat.courant() == "PLAYING" {
                        etat.en_pause = true;
                        (StatusCode::OK, "<u:PauseResponse/>".to_string())
                    } else {
                        (StatusCode::INTERNAL_SERVER_ERROR, FAUTE_701.to_string())
                    }
                } else {
                    (StatusCode::OK, "<Response/>".to_string())
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let output = DlnaOutput::new(
        "Parents test".into(),
        "uuid:5050".into(),
        host.clone(),
        format!("{host}/control"),
        format!("{host}/control"),
        None,
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Renderer {
        output,
        recues,
        etat,
        task: Serveur(task),
    }
}

#[test]
fn la_conduite_suit_l_etat_declare() {
    use ConduiteApresRefusPause::*;
    assert_eq!(
        conduite_apres_refus_pause(Some("PAUSED_PLAYBACK")),
        DejaEnPause
    );
    assert_eq!(
        conduite_apres_refus_pause(Some(" paused_playback ")),
        DejaEnPause
    );
    assert_eq!(conduite_apres_refus_pause(Some("TRANSITIONING")), Attendre);
    assert_eq!(conduite_apres_refus_pause(Some("PLAYING")), RenvoyerPause);
    assert_eq!(conduite_apres_refus_pause(Some("STOPPED")), Abandonner);
    assert_eq!(
        conduite_apres_refus_pause(Some("NO_MEDIA_PRESENT")),
        Abandonner
    );
    assert_eq!(conduite_apres_refus_pause(None), Abandonner);
}

/// Le cas du journal : la pause arrive pendant la transition d'après Seek.
#[tokio::test]
async fn une_pause_refusee_pendant_la_transition_aboutit_quand_le_renderer_en_sort() {
    let r = renderer(Scenario::TransitionPuisLecture(Duration::from_millis(700))).await;
    r.output
        .pause()
        .await
        .expect("la pause doit aboutir une fois la transition finie, pas remonter le 701");
    assert_eq!(r.etat.lock().unwrap().courant(), "PAUSED_PLAYBACK");
    assert_eq!(
        r.compte("Pause"),
        2,
        "un refus, puis une seule Pause renvoyée une fois sorti de TRANSITIONING : {:?}",
        r.recues.lock().unwrap()
    );
    assert!(r.compte("GetTransportInfo") >= 1);
}

#[tokio::test]
async fn un_renderer_deja_en_pause_n_est_pas_un_echec() {
    let r = renderer(Scenario::Fige("PAUSED_PLAYBACK")).await;
    // Le factice rend PAUSED_PLAYBACK mais refuse la pause : c'est l'état
    // qui fait foi, pas le refus.
    r.etat.lock().unwrap().en_pause = true;
    r.output.pause().await.expect("déjà en pause : succès");
    assert_eq!(r.compte("Pause"), 1, "rien à renvoyer");
}

/// Une transition qui ne finit pas : l'attente est bornée, aucune Pause
/// n'est martelée pendant qu'il transite, et le refus nomme l'état.
#[tokio::test]
async fn une_transition_qui_ne_finit_pas_rend_le_refus_en_nommant_l_etat() {
    let r = renderer(Scenario::Fige("TRANSITIONING")).await;
    let debut = Instant::now();
    let erreur = r
        .output
        .pause()
        .await
        .expect_err("jamais sorti de transition");
    let duree = debut.elapsed();
    assert!(erreur.contains("701"), "{erreur}");
    assert!(erreur.contains("TRANSITIONING"), "{erreur}");
    assert!(duree >= PAUSE_701_BUDGET, "attente écourtée : {duree:?}");
    assert!(
        duree < PAUSE_701_BUDGET + Duration::from_secs(2),
        "attente non bornée : {duree:?}"
    );
    assert_eq!(r.compte("Pause"), 1, "pas de Pause pendant la transition");
}

/// Arrêté : aucune attente n'y change rien, le refus remonte aussitôt.
#[tokio::test]
async fn un_renderer_arrete_rend_le_refus_sans_attendre() {
    let r = renderer(Scenario::Fige("STOPPED")).await;
    let debut = Instant::now();
    let erreur = r.output.pause().await.expect_err("arrêté : refus");
    assert!(erreur.contains("701"), "{erreur}");
    assert!(debut.elapsed() < Duration::from_secs(2));
    assert_eq!(r.compte("Pause"), 1);
    assert_eq!(r.compte("GetTransportInfo"), 1);
}

/// #5050 — la position mesurée pour le Seek de reprise n'est prise que d'un
/// `RelTime` lisible : ni un `RelTime` absent, ni `NOT_IMPLEMENTED` ne
/// deviennent un « 0:00 » qui ressemblerait à une mesure.
#[tokio::test]
async fn la_position_mesuree_ne_prend_qu_un_rel_time_lisible() {
    let r = renderer(Scenario::Fige("PLAYING")).await;
    assert_eq!(r.output.position_mesuree_ms().await, None, "sans RelTime");
    r.etat.lock().unwrap().rel_time = Some("NOT_IMPLEMENTED");
    assert_eq!(
        r.output.position_mesuree_ms().await,
        None,
        "NOT_IMPLEMENTED"
    );
    r.etat.lock().unwrap().rel_time = Some("0:01:20");
    assert_eq!(r.output.position_mesuree_ms().await, Some(80_000));
    r.etat.lock().unwrap().rel_time = Some("0:00:00");
    assert_eq!(r.output.position_mesuree_ms().await, Some(0));
}
