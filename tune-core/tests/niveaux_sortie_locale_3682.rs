//! Une sortie LOCALE en lecture bit-perfect doit nourrir l'analyseur de
//! spectre (#3682).
//!
//! ## Le fait
//!
//! Gilles OLIVE, fil 1173, capture du 04/09/2026 : zone « Cet ordinateur »,
//! pastille « Bit-Perfect », lecture en cours à 3:36 / 4:03, axe ISO tracé —
//! et les 32 bandes du spectre plaquées au plancher. Version, plateforme et
//! état des VU-mètres inconnus (#3682, « Ce qui n'est PAS établi »).
//!
//! ## Ce que le code dit du chemin d'une lecture locale
//!
//! Une zone locale adresse un appareil `local:…` (`LocalOutput::with_options_and_endpoint`,
//! `outputs/local.rs`). Sur ce préfixe, `reconnaitre_les_sorties`
//! (`orchestrator/resolve_local.rs`) pose `local_needs_wav` dès que le format
//! source est connu : la sortie locale ne lit PAS le fichier brut, elle reçoit
//! un WAV décodé au fil de l'eau par `transcoder_en_session`, qui attache un
//! forwarder de niveaux (`spawn_paced_levels_forwarder`) au décodeur
//! (`decode_to_pcm_streaming_tranche` → `send_windowed_pcm` après chaque
//! bloc). La pastille « Bit-Perfect » ne dit rien d'autre : le WAV servi est
//! sans perte et sans traitement, le transport ne touche aucun échantillon.
//! Ce n'est PAS le passthrough de fichier des zones réseau/PULL (#3807), et le
//! décodage-pour-niveaux de ce passthrough n'a rien à y faire.
//!
//! Il n'existait AUCUN témoin de ce chemin-là : les bancs de niveaux
//! existants couvrent le passthrough (#3807), le cache de transcodage (#3104),
//! le proxy streaming et l'avance gapless (#1541) — jamais la PREMIÈRE lecture
//! explicite d'une piste sur une zone locale, celle de la capture.
//!
//! ## Ce que ce fichier cloue
//!
//! Les ÉVÉNEMENTS `playback.audio_levels` réellement publiés sur le bus par la
//! VRAIE porte de lecture (`PlaybackOrchestrator::play`) sur une zone `local`,
//! pour de VRAIS fichiers de la caisse, un par famille de décodeur du bras
//! progressif : symphonia (FLAC, ALAC, WAV), AIFF, WavPack, APE. Chaque trame
//! doit porter un spectre de 32 bandes, et toute trame qui porte du signal
//! (crête au-dessus de −60 dBFS) doit y lever au moins une bande — exactement
//! ce que la capture montre manquant. Le silence numérique par lequel ouvrent
//! les fixtures de référence (#2218) a, lui, un spectre nul à bon droit. Le
//! PCM analysé sort du décodeur, pas du test.
//!
//! ⚠️ `tune-core` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré `[[test]]` dans `tune-core/Cargo.toml`.

use std::sync::Arc;
use tokio::sync::Mutex;

use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::models::Track;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::event_bus::EventBus;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::{NowPlaying, PlaybackManager};
use tune_core::streaming::registry::ServiceRegistry;

/// L'appareil que la zone de la capture adresse : le préfixe `local:` est ce
/// que `est_sortie_locale` lit, et c'est lui qui décide du chemin.
const APPAREIL: &str = "local:Cet ordinateur";

/// Un fichier de la caisse et ce que la ligne `tracks` en dit.
struct Fixture {
    chemin: &'static str,
    format: &'static str,
    sample_rate: u32,
    bit_depth: u16,
    /// Le NOMBRE minimal de trames de niveaux à SPECTRE NON NUL à voir. Le
    /// forwarder cadence à 40 ms de temps réel par trame, et les fixtures
    /// durent de ~0,4 s à 16 s — les fixtures de référence (#2218) ouvrent en
    /// outre sur un quart de silence numérique, dont le spectre est nul à bon
    /// droit : on exige ce que la plus courte peut donner, pas plus.
    attendus: u32,
}

/// Le FLAC 44,1/16 stéréo de la caisse (~1 s) : le cas de la capture.
const FLAC: Fixture = Fixture {
    chemin: concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
    format: "flac",
    sample_rate: 44_100,
    bit_depth: 16,
    attendus: 12,
};

/// Les autres conteneurs qu'une bibliothèque locale joue sur la même zone,
/// chacun par sa propre branche de `decode_to_pcm_streaming_inner`.
const FAMILLE: &[Fixture] = &[
    Fixture {
        chemin: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/alac/ref_16_44100_stereo.m4a"
        ),
        format: "m4a",
        sample_rate: 44_100,
        bit_depth: 16,
        attendus: 6,
    },
    Fixture {
        chemin: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wav/ref_16_44100_stereo.wav"
        ),
        format: "wav",
        sample_rate: 44_100,
        bit_depth: 16,
        attendus: 6,
    },
    Fixture {
        chemin: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/aiff/ref_16_44100_stereo.aiff"
        ),
        format: "aiff",
        sample_rate: 44_100,
        bit_depth: 16,
        attendus: 6,
    },
    Fixture {
        chemin: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wavpack/rip_16_44100_stereo.wv"
        ),
        format: "wv",
        sample_rate: 44_100,
        bit_depth: 16,
        attendus: 6,
    },
    Fixture {
        chemin: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ape/sine_16s_c3000.ape"
        ),
        format: "ape",
        sample_rate: 44_100,
        bit_depth: 16,
        attendus: 6,
    },
];

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    db: Arc<dyn DbBackend>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    rx: tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
}

/// Une base mémoire, un orchestrateur AVEC bus d'événements, une zone
/// `local` sans égaliseur, sans ReplayGain, sans plafond de fréquence : la
/// zone bit-perfect de la capture. L'appareil n'est pas enregistré — la
/// résolution locale n'en a pas besoin, et c'est elle qui arme les niveaux.
async fn banc() -> Banc {
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let playback = Arc::new(PlaybackManager::new());
    let bus = Arc::new(EventBus::new());
    let rx = bus.subscribe();
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    orch.event_bus = Some(bus);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Cet ordinateur (banc 3682)", Some("local"), Some(APPAREIL))
        .expect("zone");
    Banc {
        orch: Arc::new(orch),
        playback,
        zone_id,
        db,
        rx,
    }
}

impl Banc {
    /// Une piste locale en base pointant sur le fichier de la caisse.
    fn piste(&self, f: &Fixture) -> i64 {
        let mut t = Track::new(format!("Piste 3682 ({})", f.format));
        t.artist_name = Some("Gilles".into());
        t.album_title = Some("Bit-Perfect".into());
        t.duration_ms = 1_000;
        t.file_path = Some(f.chemin.into());
        t.format = Some(f.format.into());
        t.sample_rate = Some(f.sample_rate as i32);
        t.bit_depth = Some(f.bit_depth as i32);
        t.channels = 2;
        t.file_size = std::fs::metadata(f.chemin).ok().map(|m| m.len() as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    /// La VRAIE porte de lecture, celle que `POST /api/v1/playback/play`
    /// emprunte : `PlaybackOrchestrator::play`. Pas `resolve_queue_item_url`,
    /// porte du pré-chargement gapless, dont la première instruction
    /// (`begin_levels_prewarm`) interdit tout forwarder (voir le banc #3807).
    ///
    /// L'envoi à la sortie échoue (le registre est vide) : sans importance,
    /// le forwarder est attaché pendant la RÉSOLUTION, en amont de l'envoi.
    /// En production `transport.rs` marque la zone `Playing` une fois l'ordre
    /// parti ; ici on le fait à la main, sinon le forwarder gèle son horloge
    /// et le banc mesure zéro pour tout le monde. `play()` ne bumpe pas
    /// `play_seq` : le forwarder armé pendant la résolution survit.
    async fn jouer(&self, track_id: i64, f: &Fixture) -> Result<String, String> {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(APPAREIL.to_string()),
                track_id: Some(track_id),
                // `None` = piste de la BIBLIOTHÈQUE, ce que la file passe.
                source: None,
                source_id: None,
                title: Some("Piste 3682".into()),
                artist_name: Some("Gilles".into()),
                album_title: Some("Bit-Perfect".into()),
                cover_url: None,
                duration_ms: Some(1_000),
                seek_ms: None,
                temp_file_path: None,
                sample_rate: Some(f.sample_rate),
                bit_depth: Some(f.bit_depth),
                media_format: Some(f.format.into()),
                track_number: None,
                disc_number: None,
            })
            .await?;
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    title: "Piste 3682".into(),
                    source: "library".into(),
                    format: Some(f.format.into()),
                    sample_rate: Some(f.sample_rate),
                    bit_depth: Some(f.bit_depth as u32),
                    duration_ms: 1_000,
                    ..Default::default()
                },
            )
            .await;
        Ok(r.stream_url.unwrap_or_default())
    }

    /// Compte les `playback.audio_levels` de CETTE zone : crête maximale,
    /// trames dont le spectre a 32 bandes, trames dont le spectre porte au
    /// moins une bande non nulle, et trames qui portent du SIGNAL (crête au-
    /// dessus de −60 dBFS) sans aucune bande levée — le symptôme de la capture.
    /// S'arrête dès que `attendus` trames à spectre non nul sont atteintes.
    async fn compter_niveaux(&mut self, fenetre: std::time::Duration, attendus: u32) -> Mesure {
        let mut m = Mesure::default();
        let echeance = tokio::time::Instant::now() + fenetre;
        loop {
            let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
            if reste.is_zero() || (attendus > 0 && m.spectre_non_nul >= attendus) {
                break;
            }
            match tokio::time::timeout(reste, self.rx.recv()).await {
                Ok(Ok(ev))
                    if ev.event_type == "playback.audio_levels"
                        && ev.data.get("zone_id").and_then(|v| v.as_i64())
                            == Some(self.zone_id) =>
                {
                    m.n += 1;
                    let crete = ev
                        .data
                        .get("peak_left_db")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(f64::NEG_INFINITY);
                    m.crete = m.crete.max(crete);
                    let bandes: Vec<f64> = ev
                        .data
                        .get("spectrum")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|b| b.as_f64()).collect())
                        .unwrap_or_default();
                    if bandes.len() == 32 {
                        m.avec_32_bandes += 1;
                    }
                    let non_nul = bandes.iter().any(|b| *b > 0.0);
                    if non_nul {
                        m.spectre_non_nul += 1;
                    } else if crete > -60.0 {
                        m.signal_sans_spectre += 1;
                    }
                }
                Ok(Ok(ev)) => {
                    *m.autres.entry(ev.event_type.clone()).or_insert(0) += 1;
                }
                _ => break,
            }
        }
        m
    }
}

#[derive(Debug)]
struct Mesure {
    n: u32,
    crete: f64,
    avec_32_bandes: u32,
    spectre_non_nul: u32,
    /// Trames dont la crête dépasse −60 dBFS et dont le spectre est pourtant
    /// tout à zéro : du signal sans une bande levée, la capture de Gilles.
    signal_sans_spectre: u32,
    /// Les autres événements vus pendant la mesure : quand `n` vaut 0, c'est
    /// la seule chose qui distingue « le banc n'exerce pas le chemin » de
    /// « le chemin n'émet rien ».
    autres: std::collections::BTreeMap<String, u32>,
}

impl Default for Mesure {
    fn default() -> Self {
        Self {
            n: 0,
            crete: f64::NEG_INFINITY,
            avec_32_bandes: 0,
            spectre_non_nul: 0,
            signal_sans_spectre: 0,
            autres: std::collections::BTreeMap::new(),
        }
    }
}

/// La fenêtre d'attente : le forwarder cadence sur l'horloge de lecture, une
/// trame par 40 ms de temps réel.
const FENETRE: std::time::Duration = std::time::Duration::from_secs(25);

/// Ce que chaque fixture doit avoir produit sur le bus.
fn verifier(f: &Fixture, url: &str, m: &Mesure) {
    assert!(
        url.ends_with(".wav"),
        "[{}] une sortie LOCALE reçoit un WAV décodé au fil de l'eau \
         (`local_needs_wav` → `transcoder_en_session`) : c'est CE chemin que le \
         banc doit mesurer, parce que c'est lui qui porte le forwarder de \
         niveaux d'une lecture locale. Une URL brute signerait le passthrough \
         de fichier, où la sortie locale est réputée « décoder elle-même » et \
         où PERSONNE n'armerait de niveaux. URL : {url}",
        f.format
    );
    assert!(
        m.n >= f.attendus,
        "[{}] une zone locale bit-perfect doit émettre `playback.audio_levels` \
         pendant la lecture — c'est le seul flux qui nourrit l'analyseur de \
         spectre, les VU-mètres et la forme d'onde (#3682, Gilles OLIVE, fil \
         1173 ; #3818). Reçu {} événements, attendu au moins {} ; autres \
         événements vus : {:?}",
        f.format,
        m.n,
        f.attendus,
        m.autres
    );
    assert!(
        m.crete > -40.0,
        "[{}] les niveaux doivent décrire le SIGNAL du fichier, pas du \
         silence : crête {:.1} dBFS",
        f.format,
        m.crete
    );
    assert!(
        m.avec_32_bandes == m.n,
        "[{}] chaque trame doit porter un spectre de 32 bandes : {} sur {}",
        f.format,
        m.avec_32_bandes,
        m.n
    );
    assert!(
        m.signal_sans_spectre == 0,
        "[{}] le spectre d'une trame qui porte du signal ne peut pas être tout \
         à zéro — c'est exactement ce que la capture montre : 32 bandes au \
         plancher pendant que le morceau joue. {} trames sur {} ont une crête \
         au-dessus de −60 dBFS et aucune bande levée",
        f.format,
        m.signal_sans_spectre,
        m.n
    );
    assert!(
        m.spectre_non_nul >= f.attendus,
        "[{}] {} trames à spectre non nul sur {}, attendu au moins {} : le \
         fichier porte du signal (crête {:.1} dBFS), l'analyseur doit le montrer",
        f.format,
        m.spectre_non_nul,
        m.n,
        f.attendus,
        m.crete
    );
}

/// ⭐ #3682 — le cas de la capture : une zone locale, un FLAC 44,1/16, aucun
/// traitement. Le WAV servi est sans perte et sans DSP : « Bit-Perfect ».
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_zone_locale_bit_perfect_anime_le_spectre() {
    let mut b = banc().await;
    let id = b.piste(&FLAC);
    let url = b.jouer(id, &FLAC).await.expect("lecture");
    let m = b.compter_niveaux(FENETRE, FLAC.attendus).await;
    println!("local/flac — {url} — {m:?}");
    verifier(&FLAC, &url, &m);
}

/// Les autres conteneurs d'une bibliothèque locale : même zone, même chemin
/// (`transcoder_en_session`), mais chacun par SA branche du décodeur
/// progressif. Un garde-fou qui ne testerait que le FLAC ne garderait que la
/// branche symphonia.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_autres_conteneurs_locaux_animent_aussi_le_spectre() {
    for f in FAMILLE {
        let mut b = banc().await;
        let id = b.piste(f);
        let url = b.jouer(id, f).await.expect("lecture");
        let m = b.compter_niveaux(FENETRE, f.attendus).await;
        println!("local/{} — {url} — {m:?}", f.format);
        verifier(f, &url, &m);
    }
}
