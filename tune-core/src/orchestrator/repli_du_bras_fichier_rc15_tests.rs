//! rc15 — de bout en bout : une zone DLNA à cadence fixe dont le fichier
//! refait ne tient pas JOUE quand même. Vrai client SOAP face à un renderer
//! synthétique (même montage que #8738), FLAC synthétique, aucun appareil.
//!
//! Chaque témoin prend son propre couple de cadences : la mesure retenue par
//! l'hôte (`repli_du_bras_fichier_rc15`) est indexée par couple, et un témoin
//! ne doit pas décider pour un autre.
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SECONDES: u32 = 1;
/// La durée que la BASE donne à la piste : celle du .18 (237,6 s). C'est elle
/// qui fonde la projection ; le fichier synthétique, lui, ne dure que 1 s.
///
/// Les cadences des témoins sont plus basses que les 768 kHz du .18 : le
/// mécanisme est le même, et le rééchantillonneur en profil debug coûte
/// jusqu'à ×65 le temps réel à 768 kHz — l'arrêt du runtime attendrait la
/// fin du décodeur en flux pendant plusieurs minutes.
const DUREE_EN_BASE_MS: u32 = 237_600;
/// Le facteur temps réel mesuré sur le .18 : il tient le flux progressif
/// (≥ 1,25) mais pas un fichier de 237,6 s en 3 s.
const FACTEUR_DU_18: f64 = 1.93;
const SINK_FLAC_ET_WAV: &str =
    "http-get:*:audio/flac:*,http-get:*:audio/wav:*,http-get:*:audio/L16:*,http-get:*:audio/L24:*";
const SINK_FLAC_SEUL: &str = "http-get:*:audio/flac:*";

#[derive(Default, Debug)]
struct Etat {
    appels: Vec<String>,
    uri: String,
}

struct Serveur(tokio::task::JoinHandle<()>);
impl Drop for Serveur {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn champ(xml: &str, nom: &str) -> String {
    xml.split_once(&format!("<{nom}>"))
        .and_then(|(_, v)| v.split_once(&format!("</{nom}>")))
        .map(|(v, _)| v.replace("&amp;", "&"))
        .unwrap_or_default()
}

async fn renderer(
    udn: &str,
    sink: &'static str,
) -> (crate::outputs::dlna::DlnaOutput, Arc<Mutex<Etat>>, Serveur) {
    let state = Arc::new(Mutex::new(Etat::default()));
    let shared = state.clone();
    let app = axum::Router::new().route(
        "/control",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
            let shared = shared.clone();
            async move {
                let action = headers
                    .get("SOAPAction")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .trim_matches('"')
                    .rsplit('#')
                    .next()
                    .unwrap()
                    .to_string();
                let mut state = shared.lock().unwrap();
                state.appels.push(action.clone());
                let fields = match action.as_str() {
                    "SetAVTransportURI" => {
                        state.uri = champ(&body, "CurrentURI");
                        String::new()
                    }
                    "GetTransportInfo" => "<CurrentTransportState>PLAYING</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>".into(),
                    "GetPositionInfo" => "<Track>1</Track><TrackDuration>00:00:03</TrackDuration><TrackURI></TrackURI><RelTime>00:00:00</RelTime><AbsTime>00:00:00</AbsTime><RelCount>0</RelCount><AbsCount>0</AbsCount>".into(),
                    "GetProtocolInfo" => format!("<Source></Source><Sink>{sink}</Sink>"),
                    "GetVolume" => "<CurrentVolume>50</CurrentVolume>".into(),
                    "GetMute" => "<CurrentMute>0</CurrentMute>".into(),
                    _ => String::new(),
                };
                format!(
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{fields}</u:{action}Response></s:Body></s:Envelope>"#
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let control = format!("{host}/control");
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        crate::outputs::dlna::DlnaOutput::new(
            "DMP-A8 synthétique".into(),
            udn.into(),
            host,
            control.clone(),
            control.clone(),
            Some(control),
        ),
        state,
        Serveur(task),
    )
}

struct Banc {
    orch: PlaybackOrchestrator,
    zone: i64,
    udn: String,
    chemin: String,
    etat: Arc<Mutex<Etat>>,
    bus: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    _serveur: Serveur,
    _dir: tempfile::TempDir,
}

/// Une zone DLNA réglée sur `cadence_fixe`, une piste FLAC 16 bits de 3 s à
/// `source_hz`, un renderer qui annonce `sink`.
async fn banc(nom: &str, source_hz: u32, cadence_fixe: u32, sink: &'static str) -> Banc {
    let sqlite = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let rx = bus.subscribe();
    orch.event_bus = Some(bus);
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join(format!("{nom}.flac"));
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", source_hz, 16, 2);
    enc.start().await.unwrap();
    // Un sinus discret (−20 dBFS) : du signal, pas du silence.
    let mut pcm = Vec::with_capacity((source_hz * SECONDES * 4) as usize);
    for i in 0..(source_hz * SECONDES) {
        let x = ((2.0 * std::f64::consts::PI * 440.0 * f64::from(i) / f64::from(source_hz)).sin()
            * 3276.0) as i16;
        pcm.extend_from_slice(&x.to_le_bytes());
        pcm.extend_from_slice(&x.to_le_bytes());
    }
    enc.write(&pcm).await.unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    db.execute(
        &format!(
            "INSERT INTO tracks (id,title,file_path,format,duration_ms,sample_rate,bit_depth,channels) \
             VALUES (1,'Untitled',?,'flac',{DUREE_EN_BASE_MS},{source_hz},16,2)"
        ),
        &[&chemin as &dyn crate::db::backend::ToSqlValue],
    )
    .unwrap();
    let udn = format!("uuid:{nom}-rc15");
    let zone = ZoneRepo::with_backend(db.clone())
        .create(nom, Some("dlna"), Some(&udn))
        .unwrap();
    SettingsRepo::with_backend(db)
        .set(
            &crate::audio::frequence_fixe::cle_de_zone(zone),
            &cadence_fixe.to_string(),
        )
        .unwrap();
    let (output, etat, serveur) = renderer(&udn, sink).await;
    orch.outputs.lock().await.register(Box::new(output));
    Banc {
        orch,
        zone,
        udn,
        chemin,
        etat,
        bus: rx,
        _serveur: serveur,
        _dir: dir,
    }
}

impl Banc {
    async fn jouer(&self) -> Result<crate::orchestrator::PlayResult, String> {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.orch.play(PlayRequest {
                zone_id: self.zone,
                output_device_id: Some(self.udn.clone()),
                track_id: Some(1),
                source: Some("local".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("la lecture doit répondre bien avant le budget de 120 s du fichier refait")
    }

    /// `SetAVTransportURI` puis `Play`, et l'URI posée.
    fn uri_posee_puis_jouee(&self) -> String {
        let etat = self.etat.lock().unwrap();
        let set = etat.appels.iter().position(|a| a == "SetAVTransportURI");
        let play = etat.appels.iter().rposition(|a| a == "Play");
        assert!(
            matches!((set, play), (Some(s), Some(p)) if s < p),
            "la zone doit recevoir SetAVTransportURI PUIS Play (silence total sinon) : {:?}",
            etat.appels
        );
        etat.uri.clone()
    }

    /// La cadence réellement servie par la session installée.
    async fn cadence_servie(&self, r: &crate::orchestrator::PlayResult) -> u32 {
        assert!(r.output_sent && r.error.is_none(), "{:?}", r.error);
        let etat = self.orch.playback.get_state(self.zone).await;
        let sid = etat
            .now_playing
            .and_then(|np| np.stream_id)
            .expect("session installée");
        self.orch
            .streamer
            .stream_output_wire(&sid)
            .await
            .expect("format servi")
            .sample_rate
    }

    fn avis_de_repli(&mut self) -> Option<serde_json::Value> {
        while let Ok(ev) = self.bus.try_recv() {
            if ev.event_type == "zone.notice" && ev.data["code"] == "fixed_rate_too_slow_fallback" {
                return Some(ev.data);
            }
        }
        None
    }
}

/// Le cas du .18 : un pré-transcodage vers une cadence fixe que l'hôte ne
/// tient pas est ABANDONNÉ en vol (la mesure des premières secondes), et la
/// zone joue par le flux WAV progressif, à la même cadence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_pretranscodage_abandonne_joue_par_le_flux_progressif_rc15() {
    let mut b = banc("dmp-a8-abandon", 48_000, 96_000, SINK_FLAC_ET_WAV).await;
    super::transcodage::hote_lent_impose::imposer(&b.chemin, FACTEUR_DU_18);
    let r = b.jouer().await.expect("la zone doit jouer quelque chose");
    let uri = b.uri_posee_puis_jouee();
    assert!(uri.ends_with(".wav"), "repli = WAV progressif : {uri}");
    assert_eq!(
        b.cadence_servie(&r).await,
        96_000,
        "même cadence que demandée"
    );
    let avis = b.avis_de_repli().expect("avis zone.notice du repli");
    assert_eq!(avis["fallback"], "progressive_stream");
    assert_eq!(avis["fatal"], false);
    assert_eq!(
        super::repli_du_bras_fichier_rc15::facteur_retenu(48_000, 96_000),
        Some(FACTEUR_DU_18),
        "la mesure est retenue pour juger la piste suivante AVANT de la lancer"
    );
    b.orch.stop(b.zone, Some(&b.udn)).await;
}

/// Jugé AVANT : la mesure retenue sur cet hôte dit que le fichier ne tient
/// pas ; aucun fichier n'est lancé, le flux progressif part directement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_mesure_retenue_choisit_le_flux_progressif_avant_de_lancer_le_fichier_rc15() {
    let mut b = banc("dmp-a8-avant", 22_050, 44_100, SINK_FLAC_ET_WAV).await;
    super::repli_du_bras_fichier_rc15::retenir_le_facteur(22_050, 44_100, FACTEUR_DU_18);
    let r = b.jouer().await.expect("la zone doit jouer quelque chose");
    let uri = b.uri_posee_puis_jouee();
    assert!(uri.ends_with(".wav"), "{uri}");
    assert_eq!(b.cadence_servie(&r).await, 44_100);
    assert_eq!(b.avis_de_repli().unwrap()["fallback"], "progressive_stream");
    b.orch.stop(b.zone, Some(&b.udn)).await;
}

/// Un lecteur FLAC seul ne reçoit jamais de WAV : le repli est la cadence
/// native de la piste.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_lecteur_flac_seul_joue_a_la_cadence_native_rc15() {
    let mut b = banc("flac-seul", 24_000, 48_000, SINK_FLAC_SEUL).await;
    // 237,6 s à ×1,93 : 123 s projetées, au-delà du budget de 120 s.
    super::repli_du_bras_fichier_rc15::retenir_le_facteur(24_000, 48_000, FACTEUR_DU_18);
    let r = b.jouer().await.expect("la zone doit jouer quelque chose");
    let uri = b.uri_posee_puis_jouee();
    assert!(uri.ends_with(".flac"), "{uri}");
    assert_eq!(b.cadence_servie(&r).await, 24_000, "cadence native");
    assert_eq!(b.avis_de_repli().unwrap()["fallback"], "native_rate");
    b.orch.stop(b.zone, Some(&b.udn)).await;
}
