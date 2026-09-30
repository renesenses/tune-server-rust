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

/// Capture du journal, abonné LOCAL au fil du test (`set_default`) : le
/// runtime `#[tokio::test]` tourne sur ce seul fil, le renderer factice aussi.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Journal {
    fn write(&mut self, octets: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(octets);
        Ok(octets.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    fn abonner(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }
}

/// #5050 (diagnostic, décision du 29/09) — au refus 701, le journal dit la
/// position que le renderer annonce ET l'état de son transport. C'est la
/// mesure qui manquait aux journaux de FabienM : figé à la cible du Seek, ou
/// en lecture ? Deux refus rapprochés n'écrivent qu'une ligne.
///
/// Contre-épreuve : retirer l'appel à `journaliser_position_au_refus_701`
/// dans `pause_apres_refus_701` fait tomber ce test (aucune ligne).
#[tokio::test]
async fn un_refus_701_journalise_la_position_et_l_etat_du_transport() {
    let journal = Journal::default();
    let _garde = journal.abonner();
    let r = renderer(Scenario::Fige("TRANSITIONING")).await;
    r.etat.lock().unwrap().rel_time = Some("0:00:56");

    let evenement = ["dlna", "pause", "701", "position", "lue"].join("_");
    let premier = r.output.pause().await;
    assert!(premier.is_err(), "figé en transition : refus");

    let texte = journal.texte();
    let lignes: Vec<&str> = texte.lines().filter(|l| l.contains(&evenement)).collect();
    assert_eq!(lignes.len(), 1, "une ligne par refus :\n{texte}");
    let ligne = lignes[0];
    assert!(ligne.contains("etat=\"TRANSITIONING\""), "{ligne}");
    assert!(ligne.contains("position_ms=Some(56000)"), "{ligne}");
    assert!(ligne.contains("rel_time=\"0:00:56\""), "{ligne}");
    assert!(ligne.contains("duree_piste=\"0:05:00\""), "{ligne}");
    assert!(
        !ligne.contains("http"),
        "aucune URL dans la ligne : {ligne}"
    );
    assert_eq!(r.compte("GetPositionInfo"), 1);
}

/// Le débit est borné par renderer : deux refus à moins de
/// `DIAG_701_INTERVALLE` n'écrivent qu'une ligne. Le budget d'attente est
/// ici réduit pour que les deux refus tombent dans le même intervalle.
#[tokio::test]
async fn deux_refus_rapproches_n_ecrivent_qu_une_ligne_de_position() {
    let journal = Journal::default();
    let _garde = journal.abonner();
    let r = renderer(Scenario::Fige("TRANSITIONING")).await;
    let refus = FAUTE_701.to_string();
    let budget = Duration::from_millis(50);
    let pas = Duration::from_millis(10);
    let _ = r
        .output
        .pause_apres_refus_701(refus.clone(), budget, pas)
        .await;
    let _ = r.output.pause_apres_refus_701(refus, budget, pas).await;

    let evenement = ["dlna", "pause", "701", "position", "lue"].join("_");
    let n = journal
        .texte()
        .lines()
        .filter(|l| l.contains(&evenement))
        .count();
    assert_eq!(n, 1, "débit non borné :\n{}", journal.texte());
    assert_eq!(r.compte("GetPositionInfo"), 1);
}
