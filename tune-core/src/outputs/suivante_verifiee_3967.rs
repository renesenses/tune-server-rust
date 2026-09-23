//! #3967 — ce que le protocole permet de VÉRIFIER, éprouvé contre un vrai
//! serveur SOAP.
//!
//! Le mot de l'issue est *vérifié*. Un `200` sur `SetNextAVTransportURI` ne
//! dit que ceci : la requête était bien formée. Le journal de Villerio
//! (#4382) en est la démonstration — `dlna_set_next` acquitté, flux armé
//! réellement TIRÉ par le renderer, et malgré tout aucun enchaînement.
//!
//! AVTransport:1 offre deux témoins que l'appareil écrit LUI-MÊME, et ce sont
//! les deux seuls :
//!
//! | lecture | ce qu'elle prouve |
//! |---|---|
//! | `GetMediaInfo` → `NextURI` | l'URL qu'il RETIENT comme suivante |
//! | `GetCurrentTransportActions` → `Actions` | les actions qu'il DÉCLARE disponibles maintenant ; `Next` n'y figure que s'il se sait capable d'avancer |
//!
//! Les deux ensemble valent [`SuivantePreparee::Tenue`], et rien d'autre ne
//! le vaut. Chaque témoin ci-dessous tient un vrai renderer en face — un
//! `axum::serve`, donc **une tâche par connexion** : deux sondes de suite ne
//! peuvent pas se bloquer l'une l'autre et fabriquer un faux rouge.

use super::dlna::DlnaOutput;
use super::traits::{OutputTarget, PlayMedia, SuivantePreparee};
use axum::{Router, http::HeaderMap, routing::post};
use std::sync::{Arc, Mutex};

/// La faute SOAP d'un renderer qui n'implémente pas l'action demandée
/// (AVTransport:1, code 401 « Invalid Action »). Servie en 500 avec un corps,
/// comme le fait un vrai appareil.
const FAUTE_ACTION_ABSENTE: &str = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>401</errorCode><errorDescription>Invalid Action</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

/// Comment l'appareil se comporte. Chaque champ est une propriété d'appareil
/// observée sur le terrain, pas une commodité de témoin.
#[derive(Clone)]
struct Conduite {
    /// Retient-il la suivante qu'on lui pose ? (`false` = le 200 menteur)
    retient_la_suivante: bool,
    /// S'il en retient une AUTRE que la nôtre.
    retient_a_la_place: Option<&'static str>,
    /// Publie-t-il le champ `NextURI` dans `GetMediaInfo` ?
    publie_nexturi: bool,
    /// Ce que rend `GetCurrentTransportActions` : `None` = il ne connaît pas
    /// l'action et répond une faute 401.
    actions: Option<&'static str>,
    /// #4382 — il ACQUITTE `GetCurrentTransportActions` mais sans la balise
    /// `Actions` : une réponse 200 dont on ne peut rien tirer.
    publie_balise_actions: bool,
    /// `Next` est-il accepté, ou refusé par une faute ?
    next_accepte: bool,
}

impl Default for Conduite {
    /// Un appareil conforme et coopératif.
    fn default() -> Self {
        Self {
            retient_la_suivante: true,
            retient_a_la_place: None,
            publie_nexturi: true,
            actions: Some("Play,Stop,Pause,Seek,Next,Previous"),
            publie_balise_actions: true,
            next_accepte: true,
        }
    }
}

struct Renderer {
    output: DlnaOutput,
    /// Les `SOAPAction` reçues, dans l'ordre.
    recues: Arc<Mutex<Vec<String>>>,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

impl Renderer {
    fn actions(&self) -> Vec<String> {
        self.recues.lock().unwrap().clone()
    }
}

fn balise(xml: &str, tag: &str) -> Option<String> {
    let ouvre = format!("<{tag}>");
    let ferme = format!("</{tag}>");
    let debut = xml.find(&ouvre)? + ouvre.len();
    let fin = xml[debut..].find(&ferme)? + debut;
    Some(xml[debut..fin].to_string())
}

fn enveloppe(corps: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>{corps}</s:Body></s:Envelope>"
    )
}

async fn renderer(conduite: Conduite) -> Renderer {
    let recues: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let journal = recues.clone();
    // Ce que l'appareil RETIENT réellement comme suivante.
    let retenue: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let app = Router::new().route(
        "/control",
        post(move |entetes: HeaderMap, corps: String| {
            let journal = journal.clone();
            let retenue = retenue.clone();
            let conduite = conduite.clone();
            async move {
                let action = entetes
                    .get("SOAPAction")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .trim_matches('"')
                    .rsplit('#')
                    .next()
                    .unwrap_or("")
                    .to_string();
                journal.lock().unwrap().push(action.clone());
                match action.as_str() {
                    "SetNextAVTransportURI" => {
                        // Le 200 part TOUJOURS. C'est tout le sujet : ce que
                        // l'appareil en fait ensuite ne se lit pas ici.
                        let posee = balise(&corps, "NextURI").unwrap_or_default();
                        *retenue.lock().unwrap() = match conduite.retient_a_la_place {
                            Some(autre) => Some(autre.to_string()),
                            None if conduite.retient_la_suivante => Some(posee),
                            None => None,
                        };
                        (
                            axum::http::StatusCode::OK,
                            enveloppe("<u:SetNextAVTransportURIResponse/>"),
                        )
                    }
                    "GetMediaInfo" => {
                        let corps = if conduite.publie_nexturi {
                            let n = retenue.lock().unwrap().clone().unwrap_or_default();
                            format!(
                                "<u:GetMediaInfoResponse><NrTracks>1</NrTracks><CurrentURI>http://tune/stream/finie.wav</CurrentURI><NextURI>{n}</NextURI></u:GetMediaInfoResponse>"
                            )
                        } else {
                            // Beaucoup de piles n'exposent que le minimum.
                            "<u:GetMediaInfoResponse><NrTracks>1</NrTracks><CurrentURI>http://tune/stream/finie.wav</CurrentURI></u:GetMediaInfoResponse>".to_string()
                        };
                        (axum::http::StatusCode::OK, enveloppe(&corps))
                    }
                    "GetCurrentTransportActions" => match conduite.actions {
                        Some(_) if !conduite.publie_balise_actions => (
                            axum::http::StatusCode::OK,
                            enveloppe(
                                "<u:GetCurrentTransportActionsResponse/>",
                            ),
                        ),
                        Some(a) => (
                            axum::http::StatusCode::OK,
                            enveloppe(&format!(
                                "<u:GetCurrentTransportActionsResponse><Actions>{a}</Actions></u:GetCurrentTransportActionsResponse>"
                            )),
                        ),
                        None => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            FAUTE_ACTION_ABSENTE.to_string(),
                        ),
                    },
                    "Next" => {
                        if conduite.next_accepte {
                            (
                                axum::http::StatusCode::OK,
                                enveloppe("<u:NextResponse/>"),
                            )
                        } else {
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                FAUTE_ACTION_ABSENTE.to_string(),
                            )
                        }
                    }
                    // `SetNextAVTransportURI` annonce la durée juste après
                    // (`annoncer_duree`) : tout le reste acquitte à vide.
                    _ => (axum::http::StatusCode::OK, enveloppe("<u:Ok/>")),
                }
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hote = format!("http://{}", ecoute.local_addr().unwrap());
    let output = DlnaOutput::new(
        "DMP-A6 factice".into(),
        "uuid:3967".into(),
        hote.clone(),
        format!("{hote}/control"),
        format!("{hote}/control"),
        None,
    );
    // `axum::serve` sert UNE TÂCHE PAR CONNEXION : deux sondes de suite ne
    // peuvent pas se bloquer l'une l'autre.
    let tache = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    Renderer {
        output,
        recues,
        tache,
    }
}

const URL: &str = "http://192.168.1.196:8888/stream/9f7e6510.wav";

fn media() -> PlayMedia<'static> {
    PlayMedia {
        url: URL,
        mime_type: "audio/wav",
        title: Some("On the Run"),
        duration_ms: Some(212_000),
        ..Default::default()
    }
}

/// **Le cas nominal.** L'appareil retient notre URL et déclare `Next` :
/// verdict `Tenue`, le seul qui autorise la bascule.
#[tokio::test]
async fn tenue_quand_l_appareil_nomme_la_suivante_et_declare_next() {
    let r = renderer(Conduite::default()).await;
    r.output.set_next_media(&media()).await.unwrap();

    assert_eq!(
        r.output.suivante_preparee(URL).await,
        SuivantePreparee::Tenue
    );
    let vues = r.actions();
    assert!(
        vues.contains(&"GetMediaInfo".to_string())
            && vues.contains(&"GetCurrentTransportActions".to_string()),
        "les DEUX lectures doivent partir : {vues:?}"
    );
}

/// **Le 200 menteur, mot pour mot le cas de #4382.** L'appareil acquitte le
/// `SetNext` et n'en garde rien : `GetMediaInfo` rend un `NextURI` vide.
/// C'est `Perdue`, donc le repli.
#[tokio::test]
async fn perdue_quand_l_acquittement_ne_retient_rien() {
    let r = renderer(Conduite {
        retient_la_suivante: false,
        ..Default::default()
    })
    .await;
    // Il a bien répondu 200 : rien, dans cette réponse-là, ne trahit la perte.
    r.output.set_next_media(&media()).await.unwrap();

    assert_eq!(
        r.output.suivante_preparee(URL).await,
        SuivantePreparee::Perdue
    );
    assert!(
        !r.actions()
            .contains(&"GetCurrentTransportActions".to_string()),
        "suivante perdue : inutile de demander ses actions"
    );
}

/// Il retient une suivante, mais pas la nôtre — un point de contrôle tiers a
/// écrasé la consigne. `Perdue` : basculer ferait jouer autre chose.
#[tokio::test]
async fn perdue_quand_il_retient_une_autre_url() {
    let r = renderer(Conduite {
        retient_a_la_place: Some("http://192.168.1.50:9000/autre.flac"),
        ..Default::default()
    })
    .await;
    r.output.set_next_media(&media()).await.unwrap();

    assert_eq!(
        r.output.suivante_preparee(URL).await,
        SuivantePreparee::Perdue
    );
}

/// Il retient bien notre URL, mais ses actions courantes ne contiennent pas
/// `Next` : il ne se déclare pas capable d'avancer. `Inconnue` — on ne lui
/// commande rien, et il garde le repli d'aujourd'hui.
#[tokio::test]
async fn inconnue_quand_il_ne_declare_pas_l_action_next() {
    let r = renderer(Conduite {
        actions: Some("Play,Stop,Pause,Seek"),
        ..Default::default()
    })
    .await;
    r.output.set_next_media(&media()).await.unwrap();

    assert_eq!(
        r.output.suivante_preparee(URL).await,
        SuivantePreparee::Inconnue
    );
}

/// Il n'implémente pas `GetCurrentTransportActions` et répond une faute 401.
/// On ne conclut rien : `Inconnue`.
#[tokio::test]
async fn inconnue_quand_l_appareil_ne_connait_pas_l_action() {
    let r = renderer(Conduite {
        actions: None,
        ..Default::default()
    })
    .await;
    r.output.set_next_media(&media()).await.unwrap();

    assert_eq!(
        r.output.suivante_preparee(URL).await,
        SuivantePreparee::Inconnue
    );
}

/// Sa pile ne publie pas le champ `NextURI` du tout (#2749 : un renderer sans
/// `GetMediaInfo` exploitable ne doit rien bloquer). `Inconnue`, jamais
/// `Perdue` : l'absence de champ n'est pas une perte.
#[tokio::test]
async fn inconnue_quand_le_champ_nexturi_n_est_pas_publie() {
    let r = renderer(Conduite {
        publie_nexturi: false,
        ..Default::default()
    })
    .await;
    r.output.set_next_media(&media()).await.unwrap();

    assert_eq!(
        r.output.suivante_preparee(URL).await,
        SuivantePreparee::Inconnue
    );
}

/// La bascule envoie l'action `Next` de l'AVTransport — rien d'autre : ni
/// `SetAVTransportURI`, ni `Play`.
#[tokio::test]
async fn la_bascule_envoie_l_action_next_et_rien_d_autre() {
    let r = renderer(Conduite::default()).await;
    r.output.set_next_media(&media()).await.unwrap();
    let avant = r.actions().len();

    r.output.basculer_sur_la_suivante_preparee().await.unwrap();

    assert_eq!(
        &r.actions()[avant..],
        ["Next"],
        "une seule action, et c'est `Next`"
    );
}

/// Un `Next` refusé par une faute SOAP rend une erreur : l'appelant reprend
/// le repli, sans avoir rien cassé.
#[tokio::test]
async fn la_bascule_refusee_rend_une_erreur() {
    let r = renderer(Conduite {
        next_accepte: false,
        ..Default::default()
    })
    .await;
    r.output.set_next_media(&media()).await.unwrap();

    let issue = r.output.basculer_sur_la_suivante_preparee().await;
    assert!(
        issue.is_err_and(|e| e.contains("Next rejected") || e.contains("401")),
        "un refus doit remonter tel quel"
    );
}

/// La comparaison d'URL : dés-échappée, exacte, jamais une inclusion — sinon
/// le préfixe commun de deux flux passerait pour la même ressource.
#[test]
fn deux_url_sont_la_meme_ou_ne_le_sont_pas() {
    use super::dlna::meme_url;
    assert!(meme_url(URL, URL));
    assert!(meme_url(&format!("  {URL}  "), URL));
    assert!(meme_url("http://t/s?a=1&amp;b=2", "http://t/s?a=1&b=2"));
    assert!(!meme_url("", URL));
    assert!(!meme_url("   ", URL));
    assert!(!meme_url("http://192.168.1.196:8888/stream/", URL));
    assert!(!meme_url(URL, "http://192.168.1.196:8888/stream/"));
}

// ── #4382 — le verdict doit se DIRE, sinon le journal du testeur ne tranche pas ──
//
// Le chemin `Next` de #3967 ne s'arme que si l'appareil NOMME notre suivante
// ET DÉCLARE l'action. Jamais mesuré sur le micrologiciel 1.6.01 du DMP-A6 de
// Villerio (#4382) — et impossible à mesurer sur son rapport, parce que trois
// des cinq branches `Inconnue` n'écrivaient qu'en `debug!` et la quatrième
// n'écrivait rien du tout. Un export de journal de terrain ne porte que
// l'INFO et au-dessus : l'absence de `dlna_suivante_tenue` y était
// indiscernable d'un armement qui n'a pas eu lieu.
//
// Ces témoins capturent au niveau INFO, exactement ce qu'un testeur nous
// envoie, et exigent que chaque branche se nomme.

/// Capture d'un abonné `tracing` local au fil courant — même montage que
/// `dlna_command_tests_4258`.
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
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    /// ⚠️ INFO, pas TRACE : c'est le niveau des exports de terrain.
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

/// Le verdict relevé par `suivante_preparee`, et ce que le journal en dit au
/// niveau INFO.
async fn verdict_et_journal(conduite: Conduite) -> (SuivantePreparee, String) {
    let r = renderer(conduite).await;
    r.output.set_next_media(&media()).await.unwrap();
    let journal = Journal::default();
    let verdict = {
        let _garde = journal.abonner();
        r.output.suivante_preparee(URL).await
    };
    (verdict, journal.texte())
}

/// Sa pile ne publie pas `NextURI` : verdict `Inconnue`. Le journal doit le
/// DIRE — c'est la branche la plus probable sur un renderer minimaliste.
#[tokio::test]
async fn le_champ_nexturi_non_publie_se_nomme_au_niveau_info() {
    let (verdict, journal) = verdict_et_journal(Conduite {
        publie_nexturi: false,
        ..Default::default()
    })
    .await;

    assert_eq!(verdict, SuivantePreparee::Inconnue);
    assert!(
        journal.contains("dlna_suivante_nexturi_non_publie"),
        "un export de terrain ne porte que l'INFO : cette branche doit s'y lire.\n{journal}"
    );
}

/// Il retient bien notre suivante, mais ne connaît pas
/// `GetCurrentTransportActions` et répond une **faute SOAP 401**.
///
/// 🔴 Mesuré sur Shrek le 23/09/2026, et ce n'est pas ce qu'on attendait :
/// `av_action` rend `Ok(corps_de_la_faute)`, pas `Err`. Le refus le plus
/// probable d'un renderer minimaliste ne passe donc PAS par
/// `dlna_suivante_actions_muettes` (réservé à un échec de transport) mais par
/// la branche « pas de balise `Actions` » — celle qui, avant #4382,
/// n'écrivait **rien du tout, à aucun niveau**.
#[tokio::test]
async fn la_faute_soap_sur_les_actions_se_nomme_au_niveau_info() {
    let (verdict, journal) = verdict_et_journal(Conduite {
        actions: None,
        ..Default::default()
    })
    .await;

    assert_eq!(verdict, SuivantePreparee::Inconnue);
    assert!(
        journal.contains("dlna_suivante_actions_non_publiees"),
        "l'appareil qui refuse `GetCurrentTransportActions` doit se lire au journal.\n{journal}"
    );
}

/// Il répond à `GetCurrentTransportActions`, mais SANS la balise `Actions`.
/// Cette branche-là n'écrivait strictement rien, à aucun niveau.
#[tokio::test]
async fn une_reponse_sans_balise_actions_se_nomme_au_niveau_info() {
    let (verdict, journal) = verdict_et_journal(Conduite {
        publie_balise_actions: false,
        ..Default::default()
    })
    .await;

    assert_eq!(verdict, SuivantePreparee::Inconnue);
    assert!(
        journal.contains("dlna_suivante_actions_non_publiees"),
        "une réponse 200 sans balise `Actions` doit nommer sa branche.\n{journal}"
    );
}
