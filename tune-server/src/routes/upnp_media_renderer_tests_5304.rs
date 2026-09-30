//! #5304 — un contrôleur Tune face au renderer UPnP de Tune, par HTTP.
//!
//! Le cas de terrain : un Tune (VM Freebox) pilote la zone d'un autre Tune
//! (Futro) par son MediaRenderer intégré, et chaque changement de morceau
//! laisse un blanc de ~4 s. Le journal du contrôleur le dit en deux lignes :
//! `dlna_suivante_nexturi_non_publie` à l'armement, puis
//! `suivante_preparee=Inconnue` à la fin — le flux armé est jeté, tout est
//! relancé.
//!
//! Ici, les DEUX moitiés de production se parlent pour de vrai : le
//! `DlnaOutput` du contrôleur envoie ses requêtes SOAP par HTTP (reqwest) au
//! routeur de production du renderer (`crate::routes::router`), servi sur
//! 127.0.0.1 par `axum::serve`. Seule la sortie audio de la zone renderer
//! est factice (`MockOutput`).
use super::*;
use std::sync::atomic::{AtomicI64, Ordering};
use tune_core::outputs::dlna::DlnaOutput;
use tune_core::outputs::{OutputTarget, PlayMedia, SuivantePreparee};

const COURANTE: &str = "http://127.0.0.1:9/stream/courante-5304.flac";
/// Le flux que le contrôleur arme ~30 s avant la fin (`b5de5dc7…` au journal).
const ARMEE: &str = "http://127.0.0.1:9/stream/armee-5304.flac";

struct Banc {
    state: AppState,
    zone: i64,
    device: String,
    /// Le contrôleur : la sortie DLNA de production, pointée sur le renderer.
    controleur: DlnaOutput,
    serveur: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

impl Banc {
    async fn new() -> Self {
        // `sessions()` est une statique de processus : une plage d'ids à nous.
        static ZONE: AtomicI64 = AtomicI64::new(5_304_000);
        let zone = ZONE.fetch_add(1, Ordering::Relaxed);
        let device = format!("mock-5304-{zone}");
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
                &[&zone, &"Futro 5304", &"mock", &device],
            )
            .unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("zone_{zone}_upnp_renderer"), "true")
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                &device,
                "DAC du Futro (factice)",
            )));

        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hote = format!("http://{}", ecoute.local_addr().unwrap());
        let app = crate::routes::router(state.clone());
        let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let base = format!("{hote}{}/{zone}", upnp_renderer::RENDERER_MOUNT);
        let controleur = DlnaOutput::new(
            "Futro 5304 (Tune)".into(),
            format!("uuid:5304-{zone}"),
            hote.clone(),
            format!("{base}/AVTransport/control"),
            format!("{base}/RenderingControl/control"),
            None,
        );
        Self {
            state,
            zone,
            device,
            controleur,
            serveur,
            _dir: dir,
        }
    }

    async fn lectures_de_la_sortie(&self) -> usize {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    async fn joue_sur_le_renderer(&self) -> Option<String> {
        self.state
            .playback
            .get_state(self.zone)
            .await
            .now_playing
            .and_then(|np| np.source_id)
    }
}

fn media(url: &'static str, titre: &'static str) -> PlayMedia<'static> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        title: Some(titre),
        artist: Some("Bob Marley"),
        duration_ms: Some(298_906),
        ..Default::default()
    }
}

/// 🔴 #5304 — le renderer de Tune tient la suivante que le contrôleur de
/// Tune lui pose, le DIT, et y bascule sur `Next` sans que rien ne soit
/// relancé.
///
/// Avant le correctif, `GetMediaInfo` ne publiait pas `NextURI` : le
/// contrôleur concluait `Inconnue` (et `GetCurrentTransportActions` / `Next`
/// rendaient de toute façon une faute 401).
#[tokio::test]
async fn un_controleur_tune_voit_la_suivante_tenue_et_le_renderer_y_bascule() {
    let b = Banc::new().await;

    // Le morceau en cours, posé et lancé par le contrôleur lui-même.
    b.controleur
        .play_media(&media(COURANTE, "Bastard"))
        .await
        .expect("le renderer de Tune doit accepter SetAVTransportURI + Play");
    assert_eq!(
        b.joue_sur_le_renderer().await.as_deref(),
        Some(COURANTE),
        "le renderer doit jouer la piste posée par le contrôleur"
    );
    let lectures_avant = b.lectures_de_la_sortie().await;

    // Rien d'armé : le renderer ne prétend rien tenir.
    assert_ne!(
        b.controleur.suivante_preparee(ARMEE).await,
        SuivantePreparee::Tenue,
        "sans SetNextAVTransportURI, aucune suivante ne peut être tenue"
    );

    // L'armement, ~30 s avant la fin : `dlna_set_next`.
    b.controleur
        .set_next_media(&media(ARMEE, "No Woman, No Cry"))
        .await
        .expect("le renderer de Tune doit acquitter SetNextAVTransportURI");

    // LE point de #5304 : le verdict qui décide de garder le flux armé.
    assert_eq!(
        b.controleur.suivante_preparee(ARMEE).await,
        SuivantePreparee::Tenue,
        "#5304 : face au renderer de Tune, le contrôleur de Tune doit voir la \
         suivante TENUE (NextURI publié + action Next déclarée). Sinon il \
         conclut `Inconnue`, jette le flux armé à la fin du morceau et relance \
         tout — le blanc de ~4 s."
    );
    let media_avant = b
        .controleur
        .media_du_transport()
        .await
        .expect("GetMediaInfo doit répondre");
    assert_eq!(media_avant.courante.as_deref(), Some(COURANTE));
    assert_eq!(
        media_avant.suivante.as_deref(),
        Some(ARMEE),
        "GetMediaInfo doit nommer la suivante posée dans NextURI"
    );

    // La fin du morceau : le contrôleur demande la bascule (#3967).
    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("#5304 : le renderer de Tune doit acquitter Next quand il tient la suivante");

    // Le renderer a RÉELLEMENT enchaîné, sur le flux armé du contrôleur.
    assert_eq!(
        b.joue_sur_le_renderer().await.as_deref(),
        Some(ARMEE),
        "après Next, la zone du renderer doit jouer le flux ARMÉ — celui que \
         le contrôleur a gardé en vie — et non plus la piste finie"
    );
    assert_eq!(
        b.lectures_de_la_sortie().await,
        lectures_avant + 1,
        "Next doit lancer UNE lecture sur la sortie de la zone, pas zéro, pas deux"
    );
    let media_apres = b
        .controleur
        .media_du_transport()
        .await
        .expect("GetMediaInfo doit répondre");
    assert_eq!(
        media_apres.courante.as_deref(),
        Some(ARMEE),
        "le transport doit annoncer la suivante devenue courante : c'est ce \
         que surveille le contrôleur après sa demande de bascule"
    );
    assert_eq!(
        media_apres.suivante, None,
        "la suivante consommée ne doit plus être publiée"
    );

    // Contre-épreuve : plus rien à enchaîner — ni verdict `Tenue`, ni `Next`
    // acquitté, ni seconde lecture inventée.
    assert_ne!(
        b.controleur.suivante_preparee(ARMEE).await,
        SuivantePreparee::Tenue,
        "une suivante déjà consommée ne doit plus être déclarée tenue"
    );
    assert!(
        b.controleur
            .basculer_sur_la_suivante_preparee()
            .await
            .is_err(),
        "Next sans suivante posée doit être refusé (701), pas acquitté à vide"
    );
    assert_eq!(
        b.lectures_de_la_sortie().await,
        lectures_avant + 1,
        "un Next refusé ne doit rien relancer"
    );
    assert!(
        sessions()
            .lock()
            .unwrap()
            .get(&b.zone)
            .is_some_and(|s| s.next.is_none()),
        "la session du renderer ne doit plus rien tenir en suivante"
    );
}

/// Le SCPD annonce ce que la route sert : un point de contrôle qui lit le
/// descripteur avant d'appeler (#5304) doit y trouver les sorties `NextURI` /
/// `NextURIMetaData`, et les actions `Next` / `GetCurrentTransportActions`.
#[test]
fn le_scpd_annonce_nexturi_et_next() {
    let scpd = upnp_renderer::avtransport_scpd();
    for attendu in [
        "<name>NextURI</name><direction>out</direction>",
        "<name>NextURIMetaData</name><direction>out</direction>",
        "<action><name>Next</name>",
        "<action><name>GetCurrentTransportActions</name>",
        "<name>CurrentTransportActions</name>",
    ] {
        assert!(
            scpd.contains(attendu),
            "SCPD AVTransport sans « {attendu} »"
        );
    }
}
