//! #2742 — le motif `network_renderer_no_lpcm` dit-il vrai ? MESURÉ.
//!
//! `crossfeed_status` déclarait le crossfeed INDISPONIBLE (curseurs grisés côté
//! client) sur toute zone réseau dont le renderer n'annonce pas le LPCM, opt-in
//! `dsp_progressif_reseau` armé. Le motif supposait que le bras progressif
//! (WAV) est le SEUL chemin du crossfeed sur une zone réseau. #4849 a montré
//! que c'était faux pour l'opt-in désarmé ; ce fichier pose la même question
//! pour le renderer sans LPCM, et y répond par les OCTETS SERVIS.
//!
//! Le banc : un VRAI serveur SOAP sur `127.0.0.1` qui répond un Sink avec du
//! FLAC et AUCUN `audio/L16`, `audio/L24` ni `audio/wav` — la sonde
//! `dlna_accepte_lpcm` de l'orchestrateur répond donc « non » pour de vrai. Une
//! vraie zone DLNA, une vraie file, un vrai `resolve_queue_item_url`. Le signal
//! est une stéréo dont la voie DROITE est muette : tout ce qui sort à droite
//! dans le fichier servi est de la diaphonie, donc du crossfeed.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};

const UDN: &str = "uuid:renderer-sans-lpcm-2742";
const SERVICE: &str = "service-2742";
const PISTE: &str = "piste-2742";
const TAUX: u32 = 44_100;

/// Un renderer qui lit le FLAC, le MP3, l'AAC — mais n'annonce AUCUN PCM non
/// compressé. C'est la classe que vise `network_renderer_no_lpcm`.
const SINK_SANS_LPCM: &str = "http-get:*:audio/flac:*,\
                              http-get:*:audio/x-flac:*,\
                              http-get:*:audio/mpeg:*,\
                              http-get:*:audio/mp4:*";

/// Même serveur SOAP que le banc de #4573 : une tâche par connexion,
/// `Connection: close`, le même Sink à chaque sonde.
async fn renderer_qui_annonce(sink: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut brut = Vec::new();
                let mut tampon = [0u8; 4096];
                loop {
                    let Ok(n) = sock.read(&mut tampon).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    brut.extend_from_slice(&tampon[..n]);
                    let texte = String::from_utf8_lossy(&brut);
                    let Some(fin) = texte.find("\r\n\r\n") else {
                        continue;
                    };
                    let attendu: usize = texte
                        .lines()
                        .find_map(|l| {
                            let (nom, valeur) = l.split_once(':')?;
                            nom.eq_ignore_ascii_case("content-length")
                                .then(|| valeur.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if brut.len() >= fin + 4 + attendu {
                        break;
                    }
                }
                let corps = format!(
                    concat!(
                        r#"<?xml version="1.0"?>"#,
                        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
                        "<s:Body><u:GetProtocolInfoResponse><Sink>{}</Sink>",
                        "</u:GetProtocolInfoResponse></s:Body></s:Envelope>"
                    ),
                    sink
                );
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://127.0.0.1:{port}"), handle)
}

/// Une seconde de stéréo 44,1 kHz / 16 bits : 1 kHz à −6 dBFS à GAUCHE, un
/// silence numérique à DROITE. Encodée en FLAC par l'encodeur de la caisse.
async fn flac_voie_droite_muette(chemin: &std::path::Path) {
    let n = TAUX as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f64 / TAUX as f64;
        let l = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * 0.5;
        pcm.extend_from_slice(&((l * 32767.0) as i16).to_le_bytes());
        pcm.extend_from_slice(&0i16.to_le_bytes());
    }
    let mut enc = tune_core::audio::encoder::AudioEncoder::new("flac", TAUX, 16, 2);
    enc.start().await.unwrap();
    enc.write(&pcm).await.unwrap();
    let octets = enc.finish().await.unwrap();
    std::fs::write(chemin, octets).unwrap();
}

/// Diaphonie D/G du fichier SERVI, en dB : l'énergie de la voie droite (muette
/// à la source) rapportée à celle de la voie gauche. −∞ (plancher −240 dB)
/// quand rien n'a franchi la voie.
fn diaphonie_du_fichier_servi(chemin: &str) -> (f64, String) {
    let d = tune_core::audio::decode::decode_to_pcm(chemin, None, None, 0.0, 0.0)
        .unwrap_or_else(|e| panic!("le fichier servi doit se décoder ({chemin}) : {e}"));
    assert_eq!(d.channels, 2, "le fichier servi reste stéréo");
    let pleine_echelle = (1i64 << (d.bit_depth - 1)) as f64;
    let (mut eg, mut ed) = (0.0f64, 0.0f64);
    for f in d.samples_i32.chunks_exact(2) {
        let (l, r) = (f[0] as f64 / pleine_echelle, f[1] as f64 / pleine_echelle);
        eg += l * l;
        ed += r * r;
    }
    let db = 10.0 * (ed.max(1e-24) / eg.max(1e-24)).log10();
    let decrit = format!(
        "{} Hz / {} bits / {} voies",
        d.sample_rate, d.bit_depth, d.channels
    );
    (db, decrit)
}

/// Ce que la route `/zones/{id}/dsp` publie pour cette zone, calculé comme
/// elle (`crossfeed_status_de_zone`) : sortie, puis droits. La sonde LPCM est
/// la VRAIE, contre le renderer de ce banc.
async fn statut_publie(
    orch: &PlaybackOrchestrator,
    backend: &Arc<dyn DbBackend>,
    zone_id: i64,
) -> (bool, tune_core::audio::crossfeed::CrossfeedStatus) {
    let zone = ZoneRepo::with_backend(backend.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    let device = zone.output_device_id.clone().unwrap();
    let accepte = orch.dlna_accepte_lpcm(&device, false).await;
    let sortie = tune_core::audio::crossfeed::crossfeed_status(
        true,
        tune_core::audio::crossfeed::crossfeed_runs_on_output(Some(&device)),
        tune_core::orchestrator::is_network_output_type(zone.output_type.as_deref()),
        false,
        true,
        accepte,
    );
    (
        accepte,
        tune_core::audio::crossfeed::avec_les_droits(sortie, true, true),
    )
}

/// Un service factice qui rend un fMP4 DASH « déjà assemblé sur disque »
/// (`file://`), la forme de Tidal HI-RES — le seul bras de service qu'on
/// puisse traverser sans TLS. Il prend le même `load_streaming_dsp` que les
/// bras HTTPS (Qobuz) et AAC (YouTube).
struct ServiceDash {
    url: String,
}

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("non prévu : {quoi}"))
}

#[async_trait::async_trait]
impl StreamingService for ServiceDash {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        SERVICE
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Err(non_prevu("authenticate"))
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(non_prevu("search"))
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_prevu("get_track"))
    }
    async fn get_track_url(&self, id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        assert_eq!(id, PISTE);
        Ok(StreamUrl {
            url: self.url.clone(),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate: TAUX,
                bit_depth: 16,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
            headers: Vec::new(),
        })
    }
    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        Ok(false)
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_album_tracks"))
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_prevu("get_artist"))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_prevu("get_playlist"))
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_playlist_tracks"))
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    streamer: Arc<AudioStreamer>,
    backend: Arc<dyn DbBackend>,
    zone_id: i64,
    _renderer: tokio::task::JoinHandle<()>,
}

/// Zone DLNA vers le renderer sans LPCM, crossfeed coché (0,3 / 0,3 ms),
/// opt-in `dsp_progressif_reseau` ARMÉ — la seule configuration où le statut
/// publié porte `network_renderer_no_lpcm`.
async fn banc(services: tune_core::streaming::registry::ServiceRegistry) -> Banc {
    let (base, renderer) = renderer_qui_annonce(SINK_SANS_LPCM).await;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let zone_id = ZoneRepo::with_backend(backend.clone())
        .create("Salon sans LPCM", Some("dlna"), Some(UDN))
        .unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    settings
        .set(
            &format!("zone_{zone_id}_crossfeed"),
            r#"{"enabled":true,"amount":0.3,"delay_ms":0.3}"#,
        )
        .unwrap();
    settings.set("dsp_progressif_reseau", "true").unwrap();

    let mut registre = tune_core::outputs::registry::OutputRegistry::new();
    registre.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
        "Salon sans LPCM".into(),
        UDN.into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    )));
    let streamer = Arc::new(AudioStreamer::new(0));
    let orch = PlaybackOrchestrator::new(
        backend.clone(),
        Arc::new(tune_core::playback::PlaybackManager::new()),
        streamer.clone(),
        Arc::new(tokio::sync::Mutex::new(services)),
        Arc::new(tokio::sync::Mutex::new(registre)),
        None,
    );
    Banc {
        orch,
        streamer,
        backend,
        zone_id,
        _renderer: renderer,
    }
}

/// Le chemin sur disque des octets que la session sert au renderer.
async fn fichier_servi(streamer: &AudioStreamer, stream_id: Option<&str>) -> Option<String> {
    let sid = stream_id?;
    let sessions = streamer.sessions_state();
    let s = sessions.lock().await.get(sid).cloned()?;
    s.file_path.lock().await.clone()
}

/// ⭐ LE TÉMOIN. Flux de service (bras DASH, Tidal HI-RES) vers un renderer
/// qui n'annonce PAS le LPCM : le crossfeed est dans les octets servis — et le
/// statut publié ne doit pas le déclarer indisponible.
///
/// Rouge sur la tête de #4849 : la mesure passe (le crossfeed ATTEINT le
/// renderer, en FLAC ré-encodé), c'est le statut qui le nie — même forme que
/// le défaut de `network_progressive_off`.
#[tokio::test]
async fn un_renderer_sans_lpcm_entend_le_crossfeed_sur_un_flux_de_service_2742() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("dash-2742.flac");
    flac_voie_droite_muette(&source).await;

    let mut services = tune_core::streaming::registry::ServiceRegistry::new();
    services.register(Box::new(ServiceDash {
        url: format!("file://{}", source.display()),
    }));
    let b = banc(services).await;
    PlayQueueRepo::with_backend(b.backend.clone())
        .append(
            b.zone_id,
            &[QueueInput::Streaming {
                source: SERVICE.into(),
                source_id: PISTE.into(),
                title: "Flux 2742".into(),
                artist: "Banc".into(),
                album: None,
                cover_url: None,
                duration_ms: 1_000,
                track_number: None,
                disc_number: None,
            }],
        )
        .unwrap();

    // 0. Le renderer est bien dans la classe visée : la VRAIE sonde dit non.
    let (accepte, statut) = statut_publie(&b.orch, &b.backend, b.zone_id).await;
    assert!(
        !accepte,
        "le Sink du banc n'annonce aucun PCM : la sonde doit répondre non"
    );
    assert_eq!(
        statut.reason,
        Some(tune_core::audio::crossfeed::CrossfeedConstraint::NetworkRendererNoLpcm),
        "le banc doit produire le motif mesuré : {statut:?}"
    );

    // 1. CE QUI PART vers le renderer.
    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution du flux");
    let servi = fichier_servi(&b.streamer, r.stream_id.as_deref())
        .await
        .expect("le bras DASH sert un fichier pré-transcodé");
    let (diaphonie_db, decrit) = diaphonie_du_fichier_servi(&servi);
    println!(
        "#2742 — renderer SANS LPCM, flux de service (DASH) : servi {} ({decrit}), \
         diaphonie D/G = {diaphonie_db:.1} dB",
        r.mime_type
    );
    assert_eq!(
        r.mime_type, "audio/flac",
        "renderer FLAC : le flux part en FLAC ré-encodé, pas en LPCM"
    );
    assert!(
        diaphonie_db > -20.0,
        "le FLAC servi devait porter le crossfeed : diaphonie {diaphonie_db:.1} dB"
    );

    // 2. CE QUE L'ÉCRAN EN DIT.
    assert!(
        !statut.unavailable && statut.effective,
        "le statut publié VERROUILLE un crossfeed qui s'entend : diaphonie \
         {diaphonie_db:.1} dB dans le FLAC servi au renderer sans LPCM, et \
         pourtant {statut:?}"
    );
}

/// La piste de BIBLIOTHÈQUE, même zone, même renderer : ce qui part, et si le
/// crossfeed y est. Ce chemin appartient à un autre chantier (#2742, bras
/// fichier de `resolve_local.rs`) : le témoin ne fige PAS l'absence du
/// crossfeed. Il exige seulement que l'écran dise la vérité : si la piste
/// part sans crossfeed, le `detail` du motif doit le dire.
#[tokio::test]
async fn un_renderer_sans_lpcm_et_une_piste_de_la_bibliotheque_2742() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("bibliotheque-2742.flac");
    flac_voie_droite_muette(&source).await;

    let b = banc(tune_core::streaming::registry::ServiceRegistry::new()).await;
    let mut t = tune_core::db::models::Track::new("Piste 2742".into());
    t.duration_ms = 1_000;
    t.file_path = Some(source.to_string_lossy().into_owned());
    t.format = Some("flac".into());
    t.sample_rate = Some(TAUX as i32);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(&source).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = tune_core::db::track_repo::TrackRepo::with_backend(b.backend.clone())
        .create(&t)
        .unwrap();
    PlayQueueRepo::with_backend(b.backend.clone())
        .append(b.zone_id, &[QueueInput::Local { track_id }])
        .unwrap();

    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution de la piste");
    let servi = fichier_servi(&b.streamer, r.stream_id.as_deref())
        .await
        .unwrap_or_else(|| source.to_string_lossy().into_owned());
    let (diaphonie_db, decrit) = diaphonie_du_fichier_servi(&servi);
    let verbatim = std::fs::read(&servi).ok() == std::fs::read(&source).ok();
    println!(
        "#2742 — renderer SANS LPCM, piste de la bibliothèque : servi {} ({decrit}, \
         octets de la source verbatim : {verbatim}), diaphonie D/G = {diaphonie_db:.1} dB",
        r.mime_type
    );
    assert_ne!(
        r.mime_type, "audio/wav",
        "sans LPCM annoncé, la piste ne doit jamais partir en WAV"
    );

    let (_, statut) = statut_publie(&b.orch, &b.backend, b.zone_id).await;
    if diaphonie_db < -60.0 {
        let detail = statut.detail.unwrap_or_default();
        assert!(
            detail.contains("bibliothèque"),
            "la piste de la bibliothèque part SANS crossfeed ({diaphonie_db:.1} dB) : \
             le motif publié doit le dire, il dit « {detail} »"
        );
    }
}
