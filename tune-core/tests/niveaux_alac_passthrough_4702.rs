//! Un ALAC servi SANS CONVERSION doit animer barregraphe et crête-mètre
//! (#4702).
//!
//! ## Le fait
//!
//! Daniel LEVY, fil forum 1888 « ALAC et cretemetre », 22/09/2026 :
//! « en lecture de fichier ALAC sans conversion bargraphe et cretemetre ne
//! fonctionnent pas. » C'est tout le message : ni version, ni zone, ni
//! capture — la nouvelle interface n'a plus de page d'export des journaux.
//!
//! ## Le mécanisme
//!
//! « ALAC sans conversion » n'a qu'un sens dans le code : le **passthrough
//! ALAC**, réseau et opt-in par zone (`alac_passthrough_applies`,
//! `regles.rs`). Le fichier part alors BRUT au renderer, donc personne ne le
//! décode côté serveur — et c'est exactement le cas pour lequel le
//! décodage-pour-niveaux du passthrough existe.
//!
//! Ce décodage était sauté par :
//!
//! ```text
//! let skip_passthrough_levels = source_format
//!     .as_ref()
//!     .is_some_and(|f| f.needs_transcode_for_dlna());
//! ```
//!
//! `needs_transcode_for_dlna()` répond à « ce **renderer** saura-t-il lire ce
//! format ? » — sa propre docstring le dit, « most DLNA renderers cannot play
//! AAC ». `AudioFormat::Alac` y figure pour cette raison de renderer, pas
//! pour une raison de décodage : l'ALAC se décode parfaitement côté serveur,
//! c'est ce que fait la branche transcodée du MÊME fichier. Employer ce
//! prédicat pour décider si un fichier est DÉCODABLE éteignait donc les
//! instruments sur les deux seuls formats qui atteignent le passthrough par
//! un opt-in de zone : l'ALAC natif et l'AAC natif (#1424).
//!
//! VU-mètres, barregraphe ET crête-mètre sont nourris par un SEUL événement,
//! `playback.audio_levels` (`rms_*_db`, `peak_*_db`, `spectrum`,
//! `spectrum_db` dans la même charge utile, cf. #3807) : un seul défaut
//! d'alimentation explique les deux instruments morts ensemble, exactement
//! comme le testeur les cite ensemble.
//!
//! **Cocher « ALAC natif » éteignait les instruments**, sans que rien ne le
//! signale.
//!
//! ## Ce que ce fichier cloue
//!
//! Les ÉVÉNEMENTS réellement publiés sur le bus par la VRAIE résolution
//! (`PlaybackOrchestrator::play` → `resolve_local_track` →
//! `servir_en_passthrough`), sur un VRAI ALAC de la caisse
//! (`tests/fixtures/alac/ref_16_44100_stereo.m4a`, produit par `afconvert`,
//! contenu 100 % synthétique) — jamais la condition, qu'un test se
//! contenterait de recopier. Le PCM analysé sort du décodeur ALAC, pas du
//! test.
//!
//! Deux TÉMOINS interdisent la correction paresseuse « on arme le décodage
//! partout » et la lecture « le banc ne mesure rien » :
//!
//! 1. le même ALAC sur la MÊME zone, case **décochée** : il part transcodé en
//!    FLAC, et cette branche-là attachait déjà ses niveaux. C'est le
//!    contraste que le testeur décrit — un seul réglage sépare les deux ;
//! 2. un FLAC sur la même zone en passthrough : il animait déjà les
//!    aiguilles. Si ce témoin tombe, c'est le décodage-pour-niveaux lui-même
//!    qui est cassé, pas le prédicat de format.
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

/// Un ALAC réel de la caisse, 44,1 kHz / 16 bits / stéréo, 17 640 trames
/// (~400 ms). Le décodeur le lit vraiment, et c'est SON signal qui doit
/// remonter sur le bus. Les quatre segments (silence, deux sinus voisins,
/// bruit blanc, sinus 8 bits perdus) garantissent qu'une crête franche existe.
const ALAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/alac/ref_16_44100_stereo.m4a"
);

/// Le FLAC du second témoin : il passait déjà en passthrough avec ses niveaux.
const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");

/// La fixture ALAC dure ~400 ms.
const DUREE_MS: i64 = 400;

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    db: Arc<dyn DbBackend>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    rx: tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
}

/// Une base mémoire, un orchestrateur AVEC bus d'événements, une zone `dlna`.
/// `alac_passthrough` est l'opt-in dont le ticket parle : c'est la seule chose
/// qui sépare le cas mesuré de son premier témoin.
async fn banc(alac_passthrough: bool) -> Banc {
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
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Salon (banc 4702)", Some("dlna"), Some(RENDERER))
        .expect("zone");
    repo.update_alac_passthrough(zone_id, alac_passthrough)
        .expect("opt-in ALAC natif");
    assert_eq!(
        repo.get_alac_passthrough(zone_id),
        alac_passthrough,
        "l'opt-in de zone n'a pas été écrit : le banc mesurerait l'autre \
         branche sans le dire"
    );
    Banc {
        orch: Arc::new(orch),
        playback,
        zone_id,
        db,
        rx,
    }
}

const RENDERER: &str = "uuid:renderer-4702";

impl Banc {
    /// Une piste locale en base pointant sur un fichier de la caisse.
    fn piste(&self, chemin: &str, format: &str) -> i64 {
        let mut t = Track::new(format!("Piste 4702 ({format})"));
        t.artist_name = Some("Daniel".into());
        t.album_title = Some("Fil 1888".into());
        t.duration_ms = DUREE_MS;
        t.file_path = Some(chemin.into());
        t.format = Some(format.into());
        t.sample_rate = Some(44_100);
        t.bit_depth = Some(16);
        t.channels = 2;
        t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    /// La VRAIE porte de lecture, celle que `POST /api/v1/playback/play`
    /// emprunte : `PlaybackOrchestrator::play`.
    ///
    /// ⚠️ Et SURTOUT PAS `resolve_queue_item_url`, porte du PRÉ-CHARGEMENT
    /// gapless : sa première instruction est `begin_levels_prewarm(zone_id)`,
    /// qui interdit tout forwarder de niveaux pour la durée de l'appel. Un
    /// banc monté dessus mesure ZÉRO événement quelle que soit la zone, donc
    /// ne garde rien (leçon de #3807).
    ///
    /// L'envoi à la sortie échoue (le registre est vide) : sans importance,
    /// le décodage-pour-niveaux est armé pendant la RÉSOLUTION.
    async fn jouer(&self, track_id: i64, format: &str) -> Result<String, String> {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(RENDERER.to_string()),
                track_id: Some(track_id),
                // `None` = piste de la BIBLIOTHÈQUE : ce que la file passe.
                source: None,
                source_id: None,
                title: Some("Piste 4702".into()),
                artist_name: Some("Daniel".into()),
                album_title: Some("Fil 1888".into()),
                cover_url: None,
                duration_ms: Some(DUREE_MS),
                seek_ms: None,
                temp_file_path: None,
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                media_format: Some(format.into()),
                track_number: None,
                disc_number: None,
            })
            .await?;
        // En production, `transport.rs` marque la zone `Playing` une fois
        // l'ordre parti au renderer. Ici le registre des sorties est vide :
        // l'envoi échoue et la zone resterait `Stopped`. Or le forwarder de
        // niveaux GÈLE son horloge tant que la zone n'est pas `Playing` —
        // sans cette ligne le banc mesure zéro événement partout, témoins
        // compris, et ne garde donc rien.
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    title: "Piste 4702".into(),
                    source: "library".into(),
                    format: Some(format.into()),
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    duration_ms: DUREE_MS,
                    ..Default::default()
                },
            )
            .await;
        Ok(r.stream_url.unwrap_or_default())
    }

    /// Compte les `playback.audio_levels` de CETTE zone, la crête maximale
    /// vue et le nombre de trames portant un spectre non vide.
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

/// Le forwarder cadence sur l'HORLOGE DE LECTURE (fenêtres de ~40 ms émises
/// en temps réel) : attendre six événements coûte au moins un quart de
/// seconde de temps réel.
const FENETRE: std::time::Duration = std::time::Duration::from_secs(25);
/// La fixture ALAC dure ~400 ms, soit ~10 fenêtres de 40 ms. On en exige six :
/// assez pour distinguer « ça coule » de « rien ne vient », sans dépendre de
/// la durée exacte de la fixture ni de l'instant où la zone passe `Playing`.
const ATTENDUS: u32 = 6;

/// ⭐ #4702 — un ALAC servi sans conversion, sur une zone qui a coché
/// « ALAC natif ».
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_alac_servi_sans_conversion_anime_barregraphe_et_crete_metre() {
    let mut b = banc(true).await;
    let id = b.piste(ALAC, "alac");
    let url = b.jouer(id, "alac").await.expect("lecture");
    assert!(
        url.ends_with(".alac"),
        "le banc doit mesurer le PASSTHROUGH : l'ALAC est servi BRUT, l'URL \
         porte donc l'extension de la source. Une URL en `.flac` ou `.wav` \
         signerait le chemin TRANSCODÉ, qui attache déjà un forwarder de \
         niveaux sans rien devoir à ce correctif — le test ne garderait plus \
         rien. URL : {url}"
    );

    let m = b.compter_niveaux(FENETRE, ATTENDUS).await;
    println!("alac passthrough — {m:?}");
    assert!(
        m.n >= ATTENDUS,
        "un ALAC servi sans conversion doit animer barregraphe ET \
         crête-mètre : le décodage-pour-niveaux du passthrough était sauté \
         par `needs_transcode_for_dlna()`, où `Alac` figure pour une raison \
         de RENDERER et non de décodage (#4702, Daniel LEVY, fil 1888). \
         Reçu {} événements `playback.audio_levels`, attendu au moins \
         {ATTENDUS}. Autres événements vus : {:?}",
        m.n,
        m.autres
    );
    assert!(
        m.crete > -40.0,
        "les niveaux doivent décrire le SIGNAL du fichier, pas du silence : \
         crête {:.1} dBFS. Un forwarder qui émet des trames vides animerait \
         un compteur sans jamais bouger une aiguille",
        m.crete
    );
    assert!(
        m.avec_spectre >= ATTENDUS && m.bandes == 32,
        "le barregraphe est nourri par le MÊME événement que le crête-mètre : \
         {} trames sur {} portent un `spectrum`, de {} bandes (32 attendues)",
        m.avec_spectre,
        m.n,
        m.bandes
    );
}

/// ⭐ TÉMOIN 1 — le MÊME ALAC, la MÊME zone, case « ALAC natif » DÉCOCHÉE.
///
/// Il part alors transcodé en FLAC, et cette branche-là attachait déjà ses
/// niveaux. C'est le contraste exact que décrit le testeur : un seul réglage
/// sépare les deux lectures. Ce témoin interdit la lecture paresseuse « c'est
/// l'ALAC qui ne se décode pas ».
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temoin_le_meme_alac_transcode_animait_deja_les_instruments() {
    let mut b = banc(false).await;
    let id = b.piste(ALAC, "alac");
    let url = b.jouer(id, "alac").await.expect("lecture");
    assert!(
        !url.ends_with(".alac"),
        "sans l'opt-in, l'ALAC doit être TRANSCODÉ : une URL en `.alac` \
         signifie que le banc mesure deux fois la même branche et que le \
         contraste qu'il prétend tenir n'existe pas. URL : {url}"
    );

    let m = b.compter_niveaux(FENETRE, ATTENDUS).await;
    println!("alac transcodé (témoin 1) — {m:?}");
    assert!(
        m.n >= ATTENDUS,
        "le même ALAC transcodé animait déjà les instruments : si ce témoin \
         tombe, l'ALAC n'est pas décodable côté serveur et toute la lecture \
         du ticket s'écroule. Reçu {} événements. Autres : {:?}",
        m.n,
        m.autres
    );
    assert!(
        m.crete > -40.0,
        "le témoin doit décrire du signal : crête {:.1} dBFS",
        m.crete
    );
}

/// ⭐ TÉMOIN 2 — un FLAC sur la même zone, en passthrough.
///
/// Il animait déjà les aiguilles : `Flac` n'a jamais été dans
/// `needs_transcode_for_dlna()`. Si ce témoin tombe, c'est le
/// décodage-pour-niveaux lui-même qui est cassé, pas le prédicat de format.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temoin_un_flac_en_passthrough_animait_deja_les_aiguilles() {
    let mut b = banc(true).await;
    let id = b.piste(FLAC, "flac");
    let url = b.jouer(id, "flac").await.expect("lecture");
    assert!(url.ends_with(".flac"), "passthrough attendu, URL : {url}");

    let m = b.compter_niveaux(FENETRE, ATTENDUS).await;
    println!("flac passthrough (témoin 2) — {m:?}");
    assert!(
        m.n >= ATTENDUS,
        "un FLAC en passthrough animait déjà les aiguilles : si ce témoin \
         tombe, c'est le décodage-pour-niveaux lui-même qui est cassé. \
         Reçu {} événements. Autres : {:?}",
        m.n,
        m.autres
    );
    assert!(
        m.crete > -40.0,
        "le témoin doit décrire du signal : crête {:.1} dBFS",
        m.crete
    );
}
