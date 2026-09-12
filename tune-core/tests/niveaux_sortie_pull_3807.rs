//! Une sortie PULL en passthrough doit animer les VU-mètres (#3807).
//!
//! ## Le fait
//!
//! Dominique COMET, Tune 0.9.144 Linux, licence `diretta` active, fil 1742 :
//! « Les Vumètres et les barregraphe ne fonctionnent plus avec l'option
//! Diretta que je développe. » Le journal joint montre une lecture complète
//! sur sa zone `output_type = "diretta"` (`file_session_created`, aucune ligne
//! `transcode_*`, `total_ms=5`) : la branche **passthrough**, et l'audio, lui,
//! va jusqu'au bout.
//!
//! ## Le mécanisme
//!
//! VU-mètres ET barregraphe sont nourris par un SEUL événement,
//! `playback.audio_levels` (`tune-core/src/orchestrator.rs`), dont la charge
//! porte `rms_*_db` / `peak_*_db` **et** `spectrum` / `spectrum_db`. Un seul
//! défaut d'alimentation explique donc les deux symptômes ensemble.
//!
//! En passthrough, rien n'est décodé côté serveur : le fichier part brut. Le
//! décodage-pour-niveaux est une seconde passe montée EXPRÈS pour ce cas, et
//! elle ne s'arme que si personne d'autre ne décode. Son prédicat était
//! binaire :
//!
//! ```text
//! let output_decodes_server_side = !(is_network_output || is_browser_output);
//! ```
//!
//! `is_network_output_type` (`regles.rs`) ne nomme que `dlna`, `openhome`,
//! `chromecast`, `bluos`, `squeezebox`, `slimproto` ; `is_browser_output` ne
//! vaut que pour `browser`. Une zone `diretta` — qui reçoit une URL et va
//! chercher le flux elle-même, exactement comme une sortie réseau — tombait
//! donc dans le bras « quelqu'un décode déjà côté serveur » alors que
//! PERSONNE ne décode. Aucun `playback.audio_levels` n'était émis.
//!
//! C'est la TROISIÈME famille oubliée d'un prédicat binaire réseau/local, pour
//! la troisième fois : #1216 (Beoplay A9), puis #1430 (« égaliseur sans effet
//! vers un renderer Diretta »), maintenant les niveaux. Le prédicat de cette
//! famille existe, nommé et public — `is_pull_dsp_output_type` (`regles.rs`).
//!
//! ## Ce que ce fichier cloue
//!
//! Les ÉVÉNEMENTS réellement publiés sur le bus par la VRAIE résolution
//! (`resolve_queue_item_url` → `resolve_local_track` → `servir_en_passthrough`)
//! sur un VRAI fichier de la caisse — jamais la condition, qu'un test se
//! contenterait de recopier. Le PCM analysé sort du décodeur, pas du test :
//! c'est le fichier `tests/fixtures/test.flac` qui alimente les niveaux, et
//! les crêtes mesurées doivent décrire du SIGNAL.
//!
//! Le TÉMOIN est une zone `dlna`, qui animait déjà les aiguilles : il interdit
//! la correction paresseuse « on arme le décodage partout » comme la
//! régression inverse « on le débranche partout ».
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

/// Un FLAC réel de la caisse : le décodeur le lit vraiment, et c'est SON
/// signal qui doit remonter sur le bus.
const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    db: Arc<dyn DbBackend>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    rx: tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
}

/// Une base mémoire, un orchestrateur AVEC bus d'événements, une zone du type
/// demandé. Le `device_id` est ce que la zone croit adresser ; il n'existe pas,
/// et la résolution locale n'en a pas besoin.
async fn banc(output_type: &str, device_id: &str) -> Banc {
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
        .create("Salon (banc 3807)", Some(output_type), Some(device_id))
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
    /// Une piste locale en base pointant sur le FLAC de la caisse.
    fn piste(&self) -> i64 {
        let mut t = Track::new("Piste 3807".into());
        t.artist_name = Some("Dominique".into());
        t.album_title = Some("Diretta".into());
        t.duration_ms = 1_000;
        t.file_path = Some(FLAC.into());
        t.format = Some("flac".into());
        t.sample_rate = Some(44_100);
        t.bit_depth = Some(16);
        t.channels = 2;
        t.file_size = std::fs::metadata(FLAC).ok().map(|m| m.len() as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    /// La VRAIE porte de lecture, celle que `POST /api/v1/playback/play`
    /// emprunte : `PlaybackOrchestrator::play`.
    ///
    /// ⚠️ Et SURTOUT PAS `resolve_queue_item_url`, qui est la porte du
    /// PRÉ-CHARGEMENT gapless : sa première instruction est
    /// `begin_levels_prewarm(zone_id)`, laquelle interdit tout forwarder de
    /// niveaux pour toute la durée de l'appel (`levels_attach_allowed`). Un
    /// banc monté sur cette porte-là mesure ZÉRO événement quelle que soit la
    /// zone — y compris `dlna`, qui fonctionne — donc il ne garde rien.
    ///
    /// L'envoi à la sortie échoue (le registre est vide, l'appareil n'existe
    /// pas) : c'est sans importance ici, le décodage-pour-niveaux est armé
    /// pendant la RÉSOLUTION, en amont de l'envoi.
    async fn jouer(&self, track_id: i64, device_id: &str) -> Result<PlayResultObserve, String> {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(device_id.to_string()),
                track_id: Some(track_id),
                // `None` = piste de la BIBLIOTHEQUE : c est ce que la file passe
                // (`queue.rs`). Un `Some(..)` route vers la branche streaming.
                source: None,
                source_id: None,
                title: Some("Piste 3807".into()),
                artist_name: Some("Dominique".into()),
                album_title: Some("Diretta".into()),
                cover_url: None,
                duration_ms: Some(1_000),
                seek_ms: None,
                temp_file_path: None,
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                media_format: Some("flac".into()),
                track_number: None,
                disc_number: None,
            })
            .await?;
        // En production, `transport.rs` marque la zone `Playing` une fois
        // l'ordre parti au renderer. Ici le registre des sorties est vide :
        // l'envoi echoue (`zone.playback_error`) et la zone resterait
        // `Stopped`. Or le forwarder de niveaux GELE son horloge tant que la
        // zone n'est pas `Playing` — sans cette ligne le banc mesure zero
        // evenement pour TOUTES les zones, temoin `dlna` compris, et ne garde
        // donc rien. `play()` ne bumpe pas `play_seq` : le forwarder deja
        // arme pendant la resolution survit.
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    title: "Piste 3807".into(),
                    source: "library".into(),
                    format: Some("flac".into()),
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    duration_ms: 1_000,
                    ..Default::default()
                },
            )
            .await;
        Ok(PlayResultObserve {
            url: r.stream_url.unwrap_or_default(),
        })
    }

    /// Compte les `playback.audio_levels` de CETTE zone et rend la crête
    /// maximale vue, ainsi que le nombre de trames portant un spectre non
    /// vide. S'arrête dès que `attendus` sont atteints.
    async fn compter_niveaux(&mut self, fenetre: std::time::Duration, attendus: u32) -> Mesure {
        let mut m = Mesure::default();
        let echeance = tokio::time::Instant::now() + fenetre;
        loop {
            let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
            if reste.is_zero() || (attendus > 0 && m.n >= attendus) {
                break;
            }
            match tokio::time::timeout(reste, self.rx.recv()).await {
                Ok(Ok(ev))
                    if ev.event_type == "playback.audio_levels"
                        && ev.data.get("zone_id").and_then(|v| v.as_i64())
                            == Some(self.zone_id) =>
                {
                    m.n += 1;
                    if let Some(p) = ev.data.get("peak_left_db").and_then(|v| v.as_f64()) {
                        m.crete = m.crete.max(p);
                    }
                    let bandes = ev
                        .data
                        .get("spectrum")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    if bandes > 0 {
                        m.avec_spectre += 1;
                        m.bandes = bandes;
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

/// Ce que la lecture a rendu, réduit à ce que les témoins observent.
struct PlayResultObserve {
    url: String,
}

#[derive(Debug)]
struct Mesure {
    n: u32,
    crete: f64,
    avec_spectre: u32,
    bandes: usize,
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
            avec_spectre: 0,
            bandes: 0,
            autres: std::collections::BTreeMap::new(),
        }
    }
}

/// La fenêtre d'attente. Le forwarder cadence sur l'HORLOGE DE LECTURE
/// (fenêtres de ~40 ms émises en temps réel), donc attendre 12 événements
/// coûte au moins une demi-seconde de temps réel.
const FENETRE: std::time::Duration = std::time::Duration::from_secs(25);
/// Le FLAC de la caisse dure ~1 s, soit ~25 fenêtres de 40 ms. On en exige la
/// moitié : assez pour distinguer « ça coule » de « rien ne vient », sans
/// dépendre de la durée exacte de la fixture.
const ATTENDUS: u32 = 12;

/// ⭐ #3807 — une zone Diretta sur un FLAC 44,1/16 sans traitement.
///
/// Rien ne transcode : `pull_output_needs_dsp_transcode` ne force qu'avec un
/// égaliseur, une correction de pièce ou un ReplayGain armé. Le fichier part
/// donc BRUT, personne ne le décode côté serveur — et c'est précisément le cas
/// pour lequel le décodage-pour-niveaux existe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_zone_diretta_en_passthrough_anime_les_vu_metres() {
    let mut b = banc("diretta", "diretta:fe80--6e1f-3807").await;
    let id = b.piste();
    let r = b
        .jouer(id, "diretta:fe80--6e1f-3807")
        .await
        .expect("lecture");
    assert!(
        r.url.ends_with(".flac"),
        "le banc doit mesurer le PASSTHROUGH : le FLAC est servi BRUT, l'URL \
         porte donc l'extension du conteneur d'origine. Une URL en `.wav` ou \
         `.flac` re-encodé signerait le chemin TRANSCODÉ, qui attache déjà un \
         forwarder de niveaux sans rien devoir à ce correctif — le test ne \
         garderait plus rien. URL : {}",
        r.url
    );

    let m = b.compter_niveaux(FENETRE, ATTENDUS).await;
    println!("diretta — {m:?}");
    assert!(
        m.n >= ATTENDUS,
        "une zone Diretta en passthrough doit animer VU-mètres ET barregraphe : \
         le décodage-pour-niveaux était réservé aux sorties `is_network_output` \
         et `browser`, et une sortie PULL n'est ni l'une ni l'autre — elle \
         reçoit pourtant une URL et va chercher le flux, donc rien ne le décode \
         côté serveur (#3807, Dominique COMET, 0.9.144, fil 1742). \
         Reçu {} événements `playback.audio_levels`, attendu au moins {ATTENDUS}",
        m.n
    );
    assert!(
        m.crete > -40.0,
        "les niveaux doivent décrire le SIGNAL du fichier, pas du silence : \
         crête {:.1} dBFS. Un forwarder qui émet des trames vides animerait un \
         compteur sans jamais bouger une aiguille",
        m.crete
    );
    assert!(
        m.avec_spectre >= ATTENDUS && m.bandes == 32,
        "le barregraphe est nourri par le MÊME événement que les VU-mètres : \
         {} trames sur {} portent un `spectrum`, de {} bandes (32 attendues)",
        m.avec_spectre,
        m.n,
        m.bandes
    );
}

/// ⭐ LE TÉMOIN — la même piste, le même chemin, une zone `dlna`.
///
/// Elle animait déjà les aiguilles avant le correctif : `is_network_output`
/// la nomme. Ce témoin interdit les deux dérives symétriques — un correctif
/// qui armerait le décodage PARTOUT (il serait vert ici comme ailleurs, sans
/// rien prouver) et une régression qui le débrancherait pour tout le monde.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temoin_une_zone_dlna_en_passthrough_animait_deja_les_vu_metres() {
    let mut b = banc("dlna", "uuid:renderer-3807").await;
    let id = b.piste();
    let r = b.jouer(id, "uuid:renderer-3807").await.expect("lecture");
    assert!(
        r.url.ends_with(".flac"),
        "passthrough attendu, URL : {}",
        r.url
    );

    let m = b.compter_niveaux(FENETRE, ATTENDUS).await;
    println!("dlna (témoin) — {m:?}");
    assert!(
        m.n >= ATTENDUS,
        "une zone DLNA en passthrough animait déjà les VU-mètres : si ce témoin \
         tombe, c'est le décodage-pour-niveaux lui-même qui est cassé, pas la \
         famille PULL. Reçu {} événements",
        m.n
    );
    assert!(
        m.crete > -40.0,
        "le témoin doit décrire du signal : crête {:.1} dBFS",
        m.crete
    );
}
