//! #3183 — les deux écarts du passthrough ALAC qui restaient au tag v0.9.151.
//!
//! **Écart n° 1** : le plafond de fréquence (`max_sample_rate`, zone et
//! catalogue) ne désarmait pas « ALAC direct ». L'orchestrateur transcodait
//! quand même (le rééchantillonnage force le transcodage) mais avec
//! `alac_passthrough = true` : la négociation FLAC du renderer était sautée, et
//! le miroir du chemin du signal annonçait de l'ALAC sur un fil de FLAC. La
//! règle vit maintenant dans [`alac_passthrough_applies`], appelée des deux
//! côtés.
//!
//! **Écart n° 2** : `diretta` n'est PAS une sortie réseau, et ce n'est pas un
//! oubli. C'est une sortie PULL (greffon privé Diretta Host) : elle va
//! chercher le fichier elle-même et le décode, le Target Diretta ne reçoit que
//! des échantillons. Le passthrough n'y a pas d'objet, et l'y armer jetterait
//! l'égaliseur (#1393, Eric). Les témoins ci-dessous fixent ce comportement
//! par la porte publique, pour qu'un ajout de `diretta` à
//! `is_network_output_type` rougisse au lieu de passer en silence.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{PlaybackOrchestrator, ResolvedQueueItem, alac_passthrough_applies};
use crate::audio::formats::AudioFormat;
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const ALAC_24_96: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/alac/ref_24_96000_stereo.m4a"
);
const ALAC_16_44: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/alac/ref_16_44100_stereo.m4a"
);

// ── La règle pure ────────────────────────────────────────────────────────

/// Écart n° 1 : un ALAC 96 kHz sur une zone plafonnée à 48 kHz ne part pas
/// direct — le renderer ne le lit pas au-dessus du plafond. Et le réglage de
/// zone n'est même pas lu : la garde court-circuite avant.
#[test]
fn le_plafond_de_frequence_desarme_le_passthrough_alac() {
    let mut lu = false;
    assert!(
        !alac_passthrough_applies(
            Some("dlna"),
            Some(AudioFormat::Alac),
            96_000,
            Some(48_000),
            false,
            false,
            || {
                lu = true;
                true
            },
        ),
        "ALAC 96 kHz, plafond 48 kHz : le passthrough doit être désarmé (#3183)"
    );
    assert!(
        !lu,
        "désarmé par le plafond, le réglage de zone ne doit pas être lu"
    );
}

/// Témoin : sous le plafond, ou sans plafond, la préférence s'applique.
#[test]
fn sous_le_plafond_le_passthrough_alac_tient() {
    for (rate, cap) in [
        (96_000, Some(96_000)),
        (44_100, Some(48_000)),
        (192_000, None),
    ] {
        assert!(
            alac_passthrough_applies(
                Some("dlna"),
                Some(AudioFormat::Alac),
                rate,
                cap,
                false,
                false,
                || true,
            ),
            "{rate} Hz sous un plafond {cap:?} : le passthrough doit tenir"
        );
    }
}

/// Le plafond de fréquence est la TROISIÈME contrainte du renderer qui prime
/// sur la préférence ; les deux premières restent en place.
#[test]
fn le_forcage_wav_et_le_plafond_16_bits_desarment_toujours() {
    assert!(!alac_passthrough_applies(
        Some("dlna"),
        Some(AudioFormat::Alac),
        44_100,
        None,
        true,
        false,
        || true,
    ));
    assert!(!alac_passthrough_applies(
        Some("dlna"),
        Some(AudioFormat::Alac),
        44_100,
        None,
        false,
        true,
        || true,
    ));
}

/// Hors ALAC, ou case décochée, rien ne s'applique.
#[test]
fn sans_alac_ou_sans_opt_in_pas_de_passthrough() {
    assert!(!alac_passthrough_applies(
        Some("dlna"),
        Some(AudioFormat::Flac),
        44_100,
        None,
        false,
        false,
        || true,
    ));
    assert!(!alac_passthrough_applies(
        Some("dlna"),
        Some(AudioFormat::Alac),
        44_100,
        None,
        false,
        false,
        || false,
    ));
}

/// Écart n° 2 : les six sorties réseau portent le passthrough ; les sorties
/// PULL — `diretta` en tête — jamais, quelle que soit la case.
#[test]
fn une_zone_diretta_n_a_pas_de_passthrough_alac() {
    for t in [
        "dlna",
        "openhome",
        "chromecast",
        "bluos",
        "squeezebox",
        "slimproto",
    ] {
        assert!(
            alac_passthrough_applies(
                Some(t),
                Some(AudioFormat::Alac),
                44_100,
                None,
                false,
                false,
                || true,
            ),
            "{t} est une sortie réseau : la case s'y applique"
        );
    }
    for t in [
        Some("diretta"),
        Some("hqplayer"),
        Some("airplay2"),
        Some("oaat"),
        Some("local"),
        Some("browser"),
        None,
    ] {
        assert!(
            !alac_passthrough_applies(
                t,
                Some(AudioFormat::Alac),
                44_100,
                None,
                false,
                false,
                || true,
            ),
            "{t:?} tire le flux et le décode : le passthrough n'y a pas d'objet"
        );
    }
}

// ── Les témoins par la porte publique ────────────────────────────────────

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    zone_id: i64,
    file: PlayQueueRepo,
    position: std::cell::Cell<i64>,
}

fn banc(output_type: &str, device_id: &str) -> Banc {
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
        .create("Salon (banc 3183)", Some(output_type), Some(device_id))
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

    fn piste(&self, chemin: &str, sample_rate: i32, bit_depth: i32) -> i64 {
        let mut t = Track::new("Piste du banc 3183".into());
        t.duration_ms = 1_000;
        t.file_path = Some(chemin.into());
        t.format = Some("alac".into());
        t.sample_rate = Some(sample_rate);
        t.bit_depth = Some(bit_depth);
        t.channels = 2;
        t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    async fn resoudre(&self, track_id: i64) -> ResolvedQueueItem {
        self.file
            .append(self.zone_id, &[QueueInput::Local { track_id }])
            .expect("file");
        let position = self.position.get();
        self.position.set(position + 1);
        self.orch
            .resolve_queue_item_url(self.zone_id, position)
            .await
            .expect("résolution")
    }

    fn armer_l_eq(&self) {
        let profile = crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 80.0,
                gain: 8.0,
                q: 0.71,
                band_type: "low_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
            .set(
                &format!("zone_{}_eq_profile", self.zone_id),
                &serde_json::to_string(&profile).unwrap(),
            )
            .unwrap();
    }
}

/// Écart n° 1, le fait observable côté orchestrateur : un renderer qui REFUSE
/// le FLAC, « ALAC direct » coché, plafond 48 kHz, source ALAC 96 kHz. Le
/// passthrough est impossible (96 kHz sur un plafond 48 kHz) ; le transcodage
/// doit alors être NÉGOCIÉ comme sans opt-in — WAV, puisque le renderer ne lit
/// pas le FLAC. Avant : `alac_passthrough` restait vrai, `will_be_flac` faux,
/// la négociation sautée, et du FLAC partait vers un renderer qui n'en lit pas.
#[tokio::test]
async fn un_alac_au_dessus_du_plafond_est_negocie_comme_sans_opt_in() {
    let b = banc("dlna", "uuid:renderer-sans-flac");
    b.orch
        .dlna_unsupported_mimes
        .lock()
        .await
        .insert("uuid:renderer-sans-flac".into(), vec!["audio/flac".into()]);
    b.zones()
        .update_alac_passthrough(b.zone_id, true)
        .expect("alac_passthrough");
    b.zones()
        .update_max_sample_rate(b.zone_id, Some(48_000))
        .expect("max_sample_rate");
    let id = b.piste(ALAC_24_96, 96_000, 24);

    let r = b.resoudre(id).await;
    assert_eq!(
        r.mime_type, "audio/wav",
        "ALAC 96 kHz, plafond 48 kHz, renderer sans FLAC : le passthrough est \
         désarmé et la négociation rend du WAV (#3183, écart n° 1)"
    );
    assert_eq!(
        r.sample_rate,
        Some(48_000),
        "le plafond s'applique au flux servi"
    );

    // Témoin : plafond relevé à 96 kHz, la préférence reprend — l'ALAC part
    // tel quel, à la taille du fichier.
    b.zones()
        .update_max_sample_rate(b.zone_id, Some(96_000))
        .expect("max_sample_rate");
    let r = b.resoudre(id).await;
    assert_eq!(
        r.mime_type, "audio/mp4",
        "sous le plafond, l'ALAC part direct"
    );
    assert_eq!(
        r.file_size,
        std::fs::metadata(ALAC_24_96).ok().map(|m| m.len())
    );
}

/// Écart n° 2 : sur une zone `diretta`, la case « ALAC direct » ne change
/// rien — et c'est voulu. Sans traitement, le fichier part tel quel (la
/// sortie le décode elle-même) ; avec un égaliseur armé, il est transcodé
/// pour que l'égaliseur s'entende (#1393). Lister `diretta` parmi les sorties
/// réseau rendrait le second cas muet : `alac_passthrough` vrai, égaliseur
/// calculé puis jeté.
#[tokio::test]
async fn sur_une_zone_diretta_la_case_alac_direct_ne_change_rien() {
    let b = banc("diretta", "diretta-banc-3183");
    b.zones()
        .update_alac_passthrough(b.zone_id, true)
        .expect("alac_passthrough");
    let id = b.piste(ALAC_16_44, 44_100, 16);

    let sans_eq = b.resoudre(id).await;
    assert_eq!(
        sans_eq.mime_type, "audio/mp4",
        "sans traitement, la sortie PULL reçoit le fichier tel quel et le décode"
    );
    assert_eq!(
        sans_eq.file_size,
        std::fs::metadata(ALAC_16_44).ok().map(|m| m.len())
    );

    b.armer_l_eq();
    let avec_eq = b.resoudre(id).await;
    assert_eq!(
        avec_eq.mime_type, "audio/flac",
        "égaliseur armé : la sortie PULL doit recevoir le flux traité, case ou \
         pas (#1393) — `diretta` n'est pas une sortie réseau (#3183, écart n° 2)"
    );
}
