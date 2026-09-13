//! Témoins des blocs de `decider_la_lecture_locale` que les six témoins de
//! `temoin_resolution_locale.rs` ne traversent pas (REF-2 phase 2, #2219).
//!
//! Ces six-là jouent tous un FLAC 16 bits sur une zone DLNA, OAAT ou locale :
//! ils couvrent la relève de la source, les familles de sortie, le forçage
//! LPCM, le plafond 16 bits et la négociation DLNA — jamais le DoP anticipé,
//! le passthrough DSD, la zone navigateur, le WAV 24 bits, le passthrough
//! ALAC, le bras Chromecast, le plafond de fréquence ni le WAV progressif du
//! `.ape`. Avant de découper la méthode en temps nommés, on fixe ici ce que
//! chacun de ces blocs rend, par la même porte publique
//! (`resolve_queue_item_url`) et sur les mêmes fixtures de la caisse.
//!
//! Même montage que `temoin_resolution_locale.rs` : base mémoire, aucun
//! renderer réel. Une zone DLNA dont l'appareil est inconnu du registre est
//! réputée accepter tout MIME (`dlna_supports_mime` rend `Some(true)`) et tout
//! LPCM (`dlna_accepte_lpcm` rend `true`) — c'est le comportement de
//! production sans sonde, et celui que le témoin LAT-F1 de `tests.rs` fixe.

use std::sync::Arc;

use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;

const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");
const DSF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/dsd/ref_dsd64_stereo.dsf"
);
const ALAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/alac/ref_16_44100_stereo.m4a"
);
const AIFF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/aiff/ref_16_44100_stereo.aiff"
);
const APE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/ape/sine_16s_c3000.ape"
);

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    zone_id: i64,
    file: PlayQueueRepo,
    /// Rang du prochain ajout dans la file : chaque `resoudre` en consomme un.
    position: std::cell::Cell<i64>,
}

async fn banc(output_type: &str, device_id: &str) -> Banc {
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon (banc décision)", Some(output_type), Some(device_id))
        .expect("zone");
    Banc {
        orch,
        zone_id,
        file: PlayQueueRepo::with_backend(db.clone()),
        db,
        position: std::cell::Cell::new(0),
    }
}

impl Banc {
    fn zones(&self) -> ZoneRepo {
        ZoneRepo::with_backend(self.db.clone())
    }

    /// Une piste locale en base, dans le format `format` (ce que la ligne
    /// `tracks` dit, et dont `AudioFormat::from_extension` déduit la source).
    fn piste(&self, chemin: &str, format: &str, sample_rate: i32, bit_depth: i32) -> i64 {
        let mut t = Track::new("Piste du banc".into());
        t.artist_name = Some("Artiste du banc".into());
        t.album_title = Some("Album du banc".into());
        t.duration_ms = 1_000;
        t.file_path = Some(chemin.into());
        t.format = Some(format.into());
        t.sample_rate = Some(sample_rate);
        t.bit_depth = Some(bit_depth);
        t.channels = 2;
        t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    async fn resoudre(
        &self,
        track_id: i64,
    ) -> Result<tune_core::orchestrator::ResolvedQueueItem, String> {
        self.file
            .append(self.zone_id, &[QueueInput::Local { track_id }])
            .expect("file");
        let position = self.position.get();
        self.position.set(position + 1);
        self.orch
            .resolve_queue_item_url(self.zone_id, position)
            .await
    }
}

fn extension(url: &str) -> &str {
    url.rsplit('.').next().unwrap_or("")
}

/// Bloc DoP : une source DSD dont la sortie LOCALE est réglée « dop » est
/// résolue AVANT toute décision de transcodage — cadence DoP lue dans le
/// fichier (DSD64 → 176,4 kHz), 24 bits, WAV, sans taille (#1772, #3394).
#[tokio::test]
async fn un_dsd_en_dop_sur_une_sortie_locale_est_resolu_avant_toute_decision() {
    let b = banc("local", "local:default").await;
    b.zones()
        .update_dsd_mode(b.zone_id, "dop")
        .expect("dsd_mode");
    let id = b.piste(DSF, "dsf", 2_822_400, 1);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "le DoP voyage dans des trames WAV"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert_eq!(
        r.sample_rate,
        Some(176_400),
        "DSD64 en DoP : 2 822 400 / 16, lu dans l'en-tête du fichier"
    );
    assert_eq!(r.bit_depth, Some(24), "les trames DoP sont en 24 bits");
    assert_eq!(r.channels, Some(2));
    assert_eq!(
        r.file_size, None,
        "le flux DoP est produit au fil de l'eau : aucune taille annoncée"
    );
    assert!(r.stream_id.is_some());
}

/// Bloc `dsd_passthrough` : une zone réseau réglée « native » reçoit le DSD
/// tel quel quand la sonde ne répond pas (renderer inconnu) — MIME DSD
/// générique, taille du fichier sur disque, aucun transcodage.
#[tokio::test]
async fn un_dsd_en_mode_natif_part_tel_quel_vers_un_renderer_dlna() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    b.zones()
        .update_dsd_mode(b.zone_id, "native")
        .expect("dsd_mode");
    let id = b.piste(DSF, "dsf", 2_822_400, 1);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "application/x-dsd",
        "le renderer n'annonce rien : le MIME DSD générique, et le fichier brut"
    );
    assert_eq!(
        r.file_size,
        std::fs::metadata(DSF).ok().map(|m| m.len()),
        "en passthrough réseau, la taille annoncée est celle du fichier sur disque"
    );
    assert!(r.stream_id.is_some());
}

/// Bloc `browser_needs_wav` : une zone NAVIGATEUR ne décode pas le DSD dans
/// son `<audio>` ; la source part décodée en WAV (Reivax66, 0.9.44).
#[tokio::test]
async fn une_zone_navigateur_recoit_un_dsd_decode_en_wav() {
    let b = banc("browser", "browser:banc").await;
    let id = b.piste(DSF, "dsf", 2_822_400, 1);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "le navigateur ne lit pas le DSD brut : décodé en PCM/WAV"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert!(r.stream_id.is_some());
}

/// Bloc `dlna_wav24` : l'opt-in 24 bits sur une source plus profonde que 16
/// force le WAV, et le WAV servi garde ses 24 bits (#1137, #1654).
#[tokio::test]
async fn l_opt_in_wav_24_bits_de_la_zone_sert_un_wav_en_24_bits() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    b.zones()
        .update_dlna_wav24(b.zone_id, true)
        .expect("dlna_wav24");
    let id = b.piste(FLAC, "flac", 96_000, 24);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "`dlna_wav24` force le WAV comme `dlna_lpcm`"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert_eq!(
        r.bit_depth,
        Some(24),
        "et, à la différence de `dlna_lpcm`, le WAV servi n'est pas rabattu à 16 bits"
    );
    assert!(r.stream_id.is_some());
}

/// Bloc `alac_passthrough` : sans l'opt-in, l'ALAC est transcodé en FLAC pour
/// le réseau ; avec, il part tel quel en `audio/mp4`, à la taille du fichier.
#[tokio::test]
async fn l_opt_in_alac_de_la_zone_sert_le_fichier_tel_quel() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    let id = b.piste(ALAC, "alac", 44_100, 16);

    let sans = b.resoudre(id).await.expect("résolution sans opt-in");
    assert_eq!(
        sans.mime_type, "audio/flac",
        "sans opt-in, l'ALAC est un format « exotique » du réseau : transcodé en FLAC"
    );

    b.zones()
        .update_alac_passthrough(b.zone_id, true)
        .expect("alac_passthrough");
    let avec = b.resoudre(id).await.expect("résolution avec opt-in");
    assert_eq!(
        avec.mime_type, "audio/mp4",
        "avec l'opt-in, le conteneur ALAC part tel quel"
    );
    assert_eq!(
        avec.file_size,
        std::fs::metadata(ALAC).ok().map(|m| m.len()),
        "en passthrough réseau, la taille annoncée est celle du fichier sur disque"
    );
    assert!(avec.stream_id.is_some());
}

/// Bloc `needs_transcode_for_output` : un AIFF part tel quel vers un renderer
/// DLNA et transcodé en FLAC vers un Chromecast, dont le récepteur ne le
/// décode pas (#1210, Mika, BeoPlay A9 via CAST).
#[tokio::test]
async fn un_aiff_part_tel_quel_en_dlna_et_transcode_en_flac_vers_un_chromecast() {
    let dlna = banc("dlna", "uuid:renderer-inconnu").await;
    let id = dlna.piste(AIFF, "aiff", 44_100, 16);
    let r = dlna.resoudre(id).await.expect("résolution DLNA");
    assert_eq!(
        r.mime_type, "audio/aiff",
        "un renderer DLNA lit l'AIFF direct"
    );
    assert!(r.stream_id.is_some());

    let cast = banc("chromecast", "cast:banc").await;
    let id = cast.piste(AIFF, "aiff", 44_100, 16);
    let r = cast.resoudre(id).await.expect("résolution Chromecast");
    assert_eq!(
        r.mime_type, "audio/flac",
        "le Default Media Receiver ne décode pas l'AIFF : transcodé en FLAC"
    );
    assert_eq!(extension(&r.url), "flac", "{}", r.url);
    assert!(r.stream_id.is_some());
}

/// Bloc `needs_downsample` : le plafond de fréquence de la zone rééchantillonne
/// une source qui le dépasse, en gardant le FLAC.
#[tokio::test]
async fn le_plafond_de_frequence_de_la_zone_reechantillonne_en_gardant_le_flac() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    b.zones()
        .update_max_sample_rate(b.zone_id, Some(44_100))
        .expect("max_sample_rate");
    let id = b.piste(FLAC, "flac", 96_000, 24);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/flac",
        "le plafond ne change pas le conteneur"
    );
    assert_eq!(
        r.sample_rate,
        Some(44_100),
        "96 kHz annoncés en base, 44,1 kHz servis : le passthrough a été refusé"
    );
    assert!(r.stream_id.is_some());
}

/// Bloc `ape_flux_wav` : un `.ape` vers une zone réseau dont le renderer
/// accepte le LPCM part en WAV progressif, le seul bras qui décode le
/// Monkey's Audio au fil de l'eau (#3311, #2505).
#[tokio::test]
async fn un_ape_part_en_wav_progressif_vers_un_renderer_dlna() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    let id = b.piste(APE, "ape", 44_100, 16);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "le `.ape` réseau part en WAV progressif, pas en FLAC ré-encodé par le fichier"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert!(r.stream_id.is_some());
}
