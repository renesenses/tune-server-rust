//! Relevé par #5119 : les bras DSF UPnP (`decoder_le_dsf_distant_en_wav`) et
//! AAC (`pretranscoder_en_flac`) posent le relais DSP de la zone sur
//! `dsp.is_active()` SANS la garde « sortie locale » que porte le bras de la
//! bibliothèque (`relais_dsp_progressif`). Une zone LOCALE qui les
//! traverserait entendrait l'égaliseur deux fois : dans le flux, puis dans
//! `LocalOutput` (`apply_local_dsp`).
//!
//! Ces témoins MESURENT le PCM, par la porte publique (`resolve_stream`) :
//! zone locale, égaliseur +6 dB, une piste DSF de serveur média puis une piste
//! AAC d'un service. Ce que la sortie locale reçoit est relu et décodé, puis
//! passé dans l'égaliseur que `LocalOutput` installerait
//! (`load_eq_processor`, comme `transport.rs`) : le gain mesuré en sortie est
//! celui d'UNE passe, pas de deux.
//!
//! Chaque témoin porte son jumeau RÉSEAU (zone DLNA, même piste, même
//! égaliseur) : là, le relais s'applique et la mesure le voit. Sans ce jumeau,
//! un « rien dans le flux local » pourrait venir d'une mesure aveugle.
//!
//! La réserve automatique de l'égaliseur (`automatic_headroom_db_at`) abaisse
//! tout le signal de la norme L1 de la cascade : le niveau absolu d'un sinus à
//! 1 kHz ne vaut donc pas +6 dB. Le gain de l'égaliseur se lit en RELATIF,
//! entre le ton poussé (1 kHz) et un ton hors de la cloche (5 kHz) : la
//! réserve, uniforme, s'y annule.
use crate::TuneError;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const DSD64: u32 = 2_822_400;
const TON_POUSSE_HZ: f64 = 1_000.0;
const TON_REFERENCE_HZ: f64 = 5_000.0;
const GAIN_EQ_DB: f64 = 6.0;
const SERVICE: &str = "faux-aac";

// ── Signal ─────────────────────────────────────────────────────────────

/// Sigma-delta du 2e ordre (le modulateur de `generer_fixtures_dsd.py`),
/// deux tons : 1 kHz et 5 kHz, 0,2 chacun.
fn bits_dsd(n_bits: usize, graine: f64) -> Vec<u8> {
    let (mut i1, mut i2, mut y) = (0.0f64, 0.0f64, 1.0f64);
    let w1 = 2.0 * std::f64::consts::PI * TON_POUSSE_HZ / DSD64 as f64;
    let w2 = 2.0 * std::f64::consts::PI * TON_REFERENCE_HZ / DSD64 as f64;
    let mut octets = vec![0u8; n_bits / 8];
    for n in 0..n_bits {
        let x = 0.2 * (w1 * n as f64 + graine).sin() + 0.2 * (w2 * n as f64 + graine).sin();
        i1 += x - y;
        i2 += i1 - y;
        i1 = i1.clamp(-2.0, 2.0);
        i2 = i2.clamp(-2.0, 2.0);
        y = if i2 >= 0.0 { 1.0 } else { -1.0 };
        if y > 0.0 {
            // DSF : le bit le plus ancien en poids FAIBLE.
            octets[n / 8] |= 1 << (n % 8);
        }
    }
    octets
}

/// Un DSF stéréo DSD64 d'une demi-seconde, blocs de 4096 octets par canal.
fn dsf_deux_tons() -> Vec<u8> {
    let bloc = 4096usize;
    let octets_par_canal = bloc * 43; // ≈ 0,5 s
    let canaux = [
        bits_dsd(octets_par_canal * 8, 0.0),
        bits_dsd(octets_par_canal * 8, 0.3),
    ];
    let mut data = Vec::with_capacity(octets_par_canal * 2);
    for b in 0..octets_par_canal / bloc {
        for c in &canaux {
            data.extend_from_slice(&c[b * bloc..(b + 1) * bloc]);
        }
    }
    let total_samples = (octets_par_canal * 8) as u64;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&DSD64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&(bloc as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    buf
}

/// Un PCM décodé, en flottants entrelacés stéréo.
struct Pcm {
    sr: u32,
    samples: Vec<f32>,
}

fn decoder(octets: &[u8], extension: &str) -> Pcm {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join(format!("servi.{extension}"));
    std::fs::write(&chemin, octets).unwrap();
    let d = crate::audio::decode::decode_to_pcm(&chemin.to_string_lossy(), None, None, 0.0, 0.0)
        .unwrap();
    assert_eq!(d.channels, 2);
    let echelle = 2f64.powi(d.bit_depth as i32 - 1);
    Pcm {
        sr: d.sample_rate,
        samples: d
            .samples_i32
            .iter()
            .map(|&s| (s as f64 / echelle) as f32)
            .collect(),
    }
}

/// Amplitude d'un ton sur le canal gauche (Goertzel), en dB, en écartant le
/// premier et le dernier cinquième : transitoires du filtre et du décimateur.
fn ton_db(p: &Pcm, freq: f64) -> f64 {
    let gauche: Vec<f64> = p.samples.iter().step_by(2).map(|&s| s as f64).collect();
    let n = gauche.len();
    let tranche = &gauche[n / 5..n - n / 5];
    let w = 2.0 * std::f64::consts::PI * freq / p.sr as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in tranche {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let puissance = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    10.0 * (puissance.max(1e-30) / (tranche.len() as f64).powi(2)).log10()
}

/// Gain de l'égaliseur tel qu'on l'entend : le ton poussé contre le ton de
/// référence, relativement à la source. La réserve uniforme s'y annule.
fn gain_eq_db(p: &Pcm, source: &Pcm) -> f64 {
    (ton_db(p, TON_POUSSE_HZ) - ton_db(p, TON_REFERENCE_HZ))
        - (ton_db(source, TON_POUSSE_HZ) - ton_db(source, TON_REFERENCE_HZ))
}

/// Niveau RMS global en dB, mêmes marges.
fn rms_db(p: &Pcm) -> f64 {
    let n = p.samples.len();
    let t = &p.samples[n / 5..n - n / 5];
    let e: f64 = t.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / t.len() as f64;
    10.0 * e.max(1e-30).log10()
}

/// Ce que `LocalOutput::apply_local_dsp` fait au PCM qu'on lui sert : son
/// égaliseur, chargé comme `transport.rs` le charge.
fn passe_de_la_sortie_locale(orch: &PlaybackOrchestrator, zone_id: i64, p: &Pcm) -> Pcm {
    let mut samples = p.samples.clone();
    orch.load_eq_processor(zone_id, p.sr, 2)
        .expect("la sortie locale installe l'égaliseur de la zone")
        .process_interleaved(&mut samples);
    Pcm { sr: p.sr, samples }
}

// ── Montage ────────────────────────────────────────────────────────────

/// Un serveur HTTP qui sert un corps fixe à toute requête.
struct Serveur {
    url: String,
    _tache: tokio::task::JoinHandle<()>,
}

impl Drop for Serveur {
    fn drop(&mut self) {
        self._tache.abort();
    }
}

async fn serveur(chemin: &str, mime: &'static str, corps: Vec<u8>) -> Serveur {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{chemin}", listener.local_addr().unwrap());
    let corps = Arc::new(corps);
    let tache = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corps = corps.clone();
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let entete = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(&corps).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Serveur { url, _tache: tache }
}

/// Le service qui rend l'AAC du serveur de test.
struct ServiceAac {
    url: String,
}

#[async_trait::async_trait]
impl StreamingService for ServiceAac {
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
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Ok(url_aac(&self.url))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

fn url_aac(url: &str) -> StreamUrl {
    StreamUrl {
        url: url.to_string(),
        mime_type: "audio/mp4".into(),
        quality: StreamQuality {
            codec: "aac".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            bitrate: Some(128),
            channels: 2,
        },
        expires_at: None,
        headers: Vec::new(),
    }
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Une zone, son égaliseur +6 dB à 1 kHz, et la compensation de niveau
/// coupée : le niveau servi ne dépend alors que de l'égaliseur.
fn zone(orch: &PlaybackOrchestrator, type_sortie: &str, device: &str) -> i64 {
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zones.create("Z", Some(type_sortie), Some(device)).unwrap();
    zones.update_dsd_mode(zone_id, "pcm").unwrap();
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    let p = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: TON_POUSSE_HZ,
            gain: GAIN_EQ_DB,
            q: 1.41,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    s.set(
        &format!("zone_{zone_id}_eq_profile"),
        &serde_json::to_string(&p).unwrap(),
    )
    .unwrap();
    s.set(&format!("zone_{zone_id}_level_compensation"), "false")
        .unwrap();
    assert!(orch.zone_has_active_eq(zone_id));
    zone_id
}

/// Les octets servis : le canal d'une session, ou l'URL rendue telle quelle.
async fn octets_servis(orch: &PlaybackOrchestrator, url: &str, stream_id: Option<&str>) -> Vec<u8> {
    let Some(sid) = stream_id else {
        return crate::http::client::shared()
            .get(url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec();
    };
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session inscrite");
    // Fin du flux : le canal se ferme, ou plus rien n'arrive pendant 5 s une
    // fois des octets reçus (un relais peut garder son émetteur ouvert).
    let mut octets = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), session.recv_chunk()).await {
            Ok(Some(c)) => octets.extend_from_slice(&c),
            Ok(None) => break,
            Err(_) => {
                assert!(!octets.is_empty(), "aucun octet servi en 5 s");
                break;
            }
        }
    }
    octets
}

fn demande_upnp(zone_id: i64, device: &str) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(device.into()),
        track_id: Some(1),
        title: Some("Deux tons".into()),
        duration_ms: Some(500),
        sample_rate: Some(DSD64),
        bit_depth: Some(1),
        ..Default::default()
    }
}

fn inscrire_la_piste_upnp(orch: &PlaybackOrchestrator, url: &str) {
    orch.db
        .execute_batch(&format!(
            "INSERT INTO tracks (id,title,source,source_id,format,sample_rate,bit_depth,channels,duration_ms) \
             VALUES (1,'Deux tons','upnp','uuid:serveur|1','dsd',{DSD64},1,2,500); \
             INSERT INTO track_metadata (track_id,key,value) VALUES (1,'upnp_res_url','{url}');"
        ))
        .unwrap();
}

fn journal(cas: &str, mesure: f64, une: f64, deux: f64) {
    println!(
        "double-dsp {cas} : mesuré {mesure:.2} dB — une passe {une:.2} dB, deux passes {deux:.2} dB"
    );
}

// ── Témoins ────────────────────────────────────────────────────────────

/// DSF de serveur média. Zone LOCALE : la sortie reçoit la source intacte, et
/// l'égaliseur ne s'entend qu'UNE fois, dans `LocalOutput` (≈ +6 dB, pas +12).
/// Jumeau DLNA en `pcm` : le bras DSF décime et relaie l'égaliseur — la
/// mesure le voit (≈ +6 dB DANS le flux).
#[tokio::test]
async fn dsf_upnp_une_zone_locale_n_entend_l_egaliseur_qu_une_fois() {
    let dsf = dsf_deux_tons();
    let source = decoder(&dsf, "dsf");
    let srv = serveur("/api/v1/library/tracks/1/audio", "application/x-dsd", dsf).await;

    // ── Zone locale ──
    let orch = orchestrateur();
    inscrire_la_piste_upnp(&orch, &srv.url);
    let zid = zone(&orch, "local", "local:Casque");
    let r = orch
        .resolve_stream(&demande_upnp(zid, "local:Casque"))
        .await
        .unwrap();
    let servi = decoder(
        &octets_servis(&orch, &r.url, r.stream_id.as_deref()).await,
        if r.mime_type == "audio/wav" {
            "wav"
        } else {
            "dsf"
        },
    );
    let dans_le_flux = gain_eq_db(&servi, &source);
    let en_sortie = gain_eq_db(&passe_de_la_sortie_locale(&orch, zid, &servi), &source);
    let une = gain_eq_db(&passe_de_la_sortie_locale(&orch, zid, &source), &source);
    let deux = gain_eq_db(
        &passe_de_la_sortie_locale(&orch, zid, &passe_de_la_sortie_locale(&orch, zid, &source)),
        &source,
    );
    println!(
        "double-dsp DSF local : url={} mime={} session={:?} ; gain dans le flux {dans_le_flux:.2} dB",
        r.url, r.mime_type, r.stream_id
    );
    journal("DSF zone locale, en sortie", en_sortie, une, deux);
    assert!(
        (une - GAIN_EQ_DB).abs() < 0.5 && (deux - 2.0 * GAIN_EQ_DB).abs() < 1.0,
        "étalons faux : une passe {une:.2} dB, deux passes {deux:.2} dB"
    );
    assert!(
        dans_le_flux.abs() < 0.5,
        "le flux servi à la sortie locale porte déjà l'égaliseur : {dans_le_flux:.2} dB"
    );
    assert!(
        (en_sortie - une).abs() < 0.5,
        "zone locale : {en_sortie:.2} dB en sortie, attendu une passe ({une:.2} dB), \
         deux passes donneraient {deux:.2} dB"
    );

    // ── Jumeau réseau : la mesure voit le relais quand il existe ──
    let orch = orchestrateur();
    inscrire_la_piste_upnp(&orch, &srv.url);
    let zid = zone(&orch, "dlna", "dlna:renderer-1");
    let r = orch
        .resolve_stream(&demande_upnp(zid, "dlna:renderer-1"))
        .await
        .unwrap();
    assert!(r.stream_id.is_some(), "le bras DSF sert une session WAV");
    let servi = decoder(
        &octets_servis(&orch, &r.url, r.stream_id.as_deref()).await,
        "wav",
    );
    let reseau = gain_eq_db(&servi, &source);
    journal("DSF jumeau DLNA, dans le flux", reseau, une, deux);
    assert!(
        (reseau - une).abs() < 0.5,
        "jumeau DLNA : le relais doit porter l'égaliseur une fois ({une:.2} dB), mesuré {reseau:.2} dB"
    );
}

/// AAC d'un service. Zone LOCALE : le bras local décode en WAV sans relais,
/// l'égaliseur ne s'entend qu'UNE fois. Jumeau DLNA par le bras AAC
/// (`pretranscoder_en_flac`) : le relais s'y applique, la mesure le voit.
///
/// Le contenu du fixture AAC n'est pas choisi ici : la mesure se fait donc sur
/// le niveau RMS, étalonné par UNE et DEUX passes de l'égaliseur sur le PCM
/// source décodé par le même décodeur.
#[tokio::test]
async fn aac_une_zone_locale_n_entend_l_egaliseur_qu_une_fois() {
    let aac = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test.m4a"
    ))
    .unwrap();
    let srv = serveur("/piste.m4a", "audio/mp4", aac.clone()).await;

    // ── Zone locale, par la porte publique ──
    let orch = orchestrateur();
    orch.services.lock().await.register(Box::new(ServiceAac {
        url: srv.url.clone(),
    }));
    let zid = zone(&orch, "local", "local:Casque");
    let source = {
        // Le même décodage que le bras local, à la cadence qu'il sert.
        let d = decoder(&aac, "m4a");
        assert_eq!(d.sr, 44_100);
        d
    };
    let une = rms_db(&passe_de_la_sortie_locale(&orch, zid, &source)) - rms_db(&source);
    let deux = rms_db(&passe_de_la_sortie_locale(
        &orch,
        zid,
        &passe_de_la_sortie_locale(&orch, zid, &source),
    )) - rms_db(&source);
    assert!(
        (deux - une).abs() > 3.0,
        "étalons trop proches pour distinguer une passe de deux : {une:.2} / {deux:.2} dB"
    );
    let req = PlayRequest {
        zone_id: zid,
        output_device_id: Some("local:Casque".into()),
        source: Some(SERVICE.into()),
        source_id: Some("1".into()),
        title: Some("AAC".into()),
        duration_ms: Some(1_000),
        ..Default::default()
    };
    let r = orch.resolve_stream(&req).await.unwrap();
    let servi = decoder(
        &octets_servis(&orch, &r.url, r.stream_id.as_deref()).await,
        "wav",
    );
    let dans_le_flux = rms_db(&servi) - rms_db(&source);
    let en_sortie = rms_db(&passe_de_la_sortie_locale(&orch, zid, &servi)) - rms_db(&source);
    println!(
        "double-dsp AAC local : mime={} session={:?} ; écart dans le flux {dans_le_flux:.2} dB",
        r.mime_type, r.stream_id
    );
    journal("AAC zone locale, en sortie", en_sortie, une, deux);
    assert!(
        dans_le_flux.abs() < 0.5,
        "le flux servi à la sortie locale porte déjà l'égaliseur : {dans_le_flux:.2} dB"
    );
    assert!(
        (en_sortie - une).abs() < 0.5,
        "zone locale : {en_sortie:.2} dB en sortie, attendu une passe ({une:.2} dB), \
         deux passes donneraient {deux:.2} dB"
    );

    // ── Jumeau réseau, par le bras AAC lui-même ──
    let orch = orchestrateur();
    let zid = zone(&orch, "dlna", "dlna:renderer-1");
    let req = PlayRequest {
        zone_id: zid,
        output_device_id: Some("dlna:renderer-1".into()),
        source: Some(SERVICE.into()),
        source_id: Some("1".into()),
        ..Default::default()
    };
    let (url, sid, _, _) = orch
        .pretranscoder_en_flac(&req, SERVICE, &url_aac(&srv.url), "aac".into(), None)
        .await
        .unwrap();
    let servi = decoder(&octets_servis(&orch, &url, sid.as_deref()).await, "wav");
    let reseau = rms_db(&servi) - rms_db(&source);
    journal("AAC jumeau DLNA, dans le flux", reseau, une, deux);
    assert!(
        (reseau - une).abs() < 0.5,
        "jumeau DLNA : le relais doit porter l'égaliseur une fois ({une:.2} dB), mesuré {reseau:.2} dB"
    );
}
