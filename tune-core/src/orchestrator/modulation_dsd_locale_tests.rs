//! rc14 — TESS vers le DSD sur la sortie locale, de la chaîne de la zone aux
//! octets de la session : non-régression du PCM à l'octet près, porteur DoP
//! de bout en bout, replis dits.

use super::*;
use std::sync::Arc;
use std::time::Duration;

use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::tess::chaine::{ChaineTess, EtagesFixes, TauxDsd, Upsampling};
use crate::tess::modulation_dsd::{EtatModulation, MotifDsdRefuse, TransportModulation};

const FLAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_16_44100_stereo.flac"
);

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    track_id: i64,
    /// Le registre de `tess::modulation_dsd` est global, et chaque banc a sa
    /// base neuve (zone 1, piste 1) : les bancs passent un par un.
    _tour: tokio::sync::MutexGuard<'static, ()>,
}

static TOUR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Zone et piste du banc : loin des identifiants des autres bancs.
const ID_DU_BANC: i64 = 9_430_077;

async fn banc(premium: bool, dsd_mode: &str) -> Banc {
    let tour = TOUR.lock().await;
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
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
    orch.license = Some(if premium {
        crate::license::licence_premium_pour_test(db.clone())
    } else {
        // Sans compte Premium : la licence existe et dit Free (sans licence du
        // tout, l'hôte se tient pour Premium, comme le suréchantillonneur).
        let l = Arc::new(crate::license::LicenseManager::new(db.clone()));
        assert!(!l.premium_snapshot());
        l
    });
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create(
            "DAC DSD (banc rc14)",
            Some("local"),
            Some("local:DAC DSD banc"),
        )
        .unwrap();
    // Le registre est global, et toute résolution locale d'un autre banc
    // (zone 1, piste 1) y passe : des identifiants à part.
    db.execute(
        "UPDATE zones SET id = ? WHERE id = ?",
        &[&ID_DU_BANC, &zone_id],
    )
    .unwrap();
    let zone_id = ID_DU_BANC;
    ZoneRepo::with_backend(db.clone())
        .update_dsd_mode(zone_id, dsd_mode)
        .unwrap();
    let s = SettingsRepo::with_backend(db);
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
    orch.playback.set_volume(zone_id, 1.0).await;
    // La machine de test (debug) n'est pas celle de la production : une
    // mesure imposée, au-dessus de la marge.
    crate::tess::modulation_dsd::retenir_mesure_de_la_machine([50.0, 25.0, 12.0]);
    let mut t = Track::new("Piste du banc DSD rc14".into());
    t.duration_ms = 1_000;
    t.file_path = Some(FLAC.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(FLAC).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = TrackRepo::with_backend(orch.db.clone()).create(&t).unwrap();
    orch.db
        .execute(
            "UPDATE tracks SET id = ? WHERE id = ?",
            &[&ID_DU_BANC, &track_id],
        )
        .unwrap();
    let track_id = ID_DU_BANC;
    Banc {
        orch,
        zone_id,
        track_id,
        _tour: tour,
    }
}

fn ecrire_chaine(b: &Banc, upsampling: Option<Upsampling>) {
    let s = SettingsRepo::with_backend(b.orch.db.clone());
    match upsampling {
        None => {
            let _ = s.delete(&crate::tess::cle_chaine_zone(b.zone_id));
        }
        Some(u) => {
            let chaine = ChaineTess {
                fixed: EtagesFixes {
                    upsampling: u,
                    ..EtagesFixes::default()
                },
                ..ChaineTess::default()
            };
            crate::tess::migration::ecrire_chaine(&s, b.zone_id, &chaine).unwrap();
        }
    }
}

/// Résout la piste (FLAC 16/44,1 stéréo de la caisse) et rend (mime, octets
/// complets de la session, id de la piste).
async fn jouer(b: &Banc) -> (String, Vec<u8>, i64) {
    let track_id = b.track_id;
    let pq = PlayQueueRepo::with_backend(b.orch.db.clone());
    let _ = pq.clear(b.zone_id);
    pq.append(b.zone_id, &[QueueInput::Local { track_id }])
        .unwrap();
    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution");
    let sid = r.stream_id.clone().expect("une session");
    let session = b
        .orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session inscrite");
    let mut octets = Vec::new();
    if let Some(chemin) = session.file_path.lock().await.clone() {
        octets = std::fs::read(chemin).unwrap();
    } else {
        while let Ok(Some(c)) =
            tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await
        {
            octets.extend_from_slice(&c);
        }
    }
    (r.mime_type, octets, track_id)
}

fn dsd(rate: TauxDsd) -> Upsampling {
    Upsampling::Dsd {
        rate,
        filter: crate::audio::surechantillonnage::greffon::DEFAULT_FILTER.into(),
    }
}

/// LE témoin de non-régression : avec `pcm`, `off` ou sans chaîne, la zone
/// sert EXACTEMENT les mêmes octets — alors même que tout est réuni pour que
/// le DSD s'engage (Premium, moteur installé, zone « dop », volume 100 %).
/// La contre-épreuve : la même zone en `dsd` sert autre chose.
#[tokio::test]
async fn pcm_off_et_sans_chaine_servent_les_memes_octets() {
    let b = banc(true, "dop").await;
    ecrire_chaine(&b, None);
    let (mime_0, ref_0, piste) = jouer(&b).await;
    assert!(ref_0.len() > 44, "la référence doit porter du PCM");
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)),
        None
    );
    for u in [
        Upsampling::Off {},
        Upsampling::Pcm {
            filter: crate::audio::surechantillonnage::greffon::DEFAULT_FILTER.into(),
            max_ratio: Some(4),
            max_rate_hz: None,
        },
    ] {
        ecrire_chaine(&b, Some(u.clone()));
        let (mime, octets, piste) = jouer(&b).await;
        assert_eq!(mime, mime_0, "{u:?}");
        assert!(octets == ref_0, "{u:?} : les octets servis ont changé");
        assert_eq!(
            crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)),
            None,
            "{u:?}"
        );
    }
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let (_, octets, _) = jouer(&b).await;
    assert!(
        octets != ref_0,
        "contre-épreuve : le DSD doit changer le flux"
    );
}

/// La piste PCM part en DoP DSD64 : en-tête WAV 24 bits à 176,4 kHz, marqueurs
/// alternés sur TOUT le flux et identiques sur les deux voies, débit attendu,
/// état publié pour le chemin du signal.
#[tokio::test]
async fn une_piste_pcm_part_en_dop_dsd64_de_bout_en_bout() {
    let b = banc(true, "dop").await;
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let (mime, octets, piste) = jouer(&b).await;
    assert_eq!(mime, "audio/wav");
    assert_eq!(&octets[..4], b"RIFF");
    let cadence = u32::from_le_bytes(octets[24..28].try_into().unwrap());
    let canaux = u16::from_le_bytes(octets[22..24].try_into().unwrap());
    let bits = u16::from_le_bytes(octets[34..36].try_into().unwrap());
    assert_eq!((cadence, canaux, bits), (176_400, 2, 24));
    let charge = &octets[44..];
    assert_eq!(charge.len() % 6, 0);
    let trames = charge.len() / 6;
    // ≈ 1 s de piste (4 trames DoP par trame PCM), plus la queue du filtre.
    let trames_pcm = {
        let r = crate::audio::decode::decode_to_pcm(FLAC, None, None, 0.0, 0.0).unwrap();
        r.samples_i32.len() / 2
    };
    assert!(trames >= trames_pcm * 4, "{trames} < {}", trames_pcm * 4);
    assert!(trames <= trames_pcm * 4 + 1_000, "{trames}");
    assert_eq!(trames % 2, 0, "clôture paire (#7704)");
    let mut attendu = 0x05u8;
    for t in charge.as_chunks::<6>().0 {
        assert_eq!((t[2], t[5]), (attendu, attendu));
        attendu = if attendu == 0x05 { 0xFA } else { 0x05 };
    }
    #[cfg(feature = "local-audio")]
    assert!(crate::outputs::local::is_dop_pcm(charge, 24, 2));
    match crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)) {
        Some(EtatModulation::EnService(m)) => {
            assert_eq!(m.transport, TransportModulation::Dop);
            assert_eq!(
                (m.rate_hz, m.carrier_rate_hz, m.order),
                (2_822_400, 176_400, 7)
            );
        }
        autre => panic!("état {autre:?}"),
    }
}

/// Une zone qui n'a pas déclaré son DAC DSD (`auto`) : rien en DoP, la piste
/// part en PCM comme avant, et le motif est retenu.
#[tokio::test]
async fn zone_auto_repli_en_pcm_dit() {
    let b = banc(true, "dop").await;
    ecrire_chaine(&b, None);
    let (_, reference, _) = jouer(&b).await;
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_dsd_mode(b.zone_id, "auto")
        .unwrap();
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd128)));
    let (_, octets, piste) = jouer(&b).await;
    assert!(octets == reference, "le repli sert le PCM d'avant");
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)),
        Some(EtatModulation::Repli(MotifDsdRefuse::DsdModeNotEnabled))
    );
}

/// La protection du niveau DSD (#8813) : volume à 50 %, la piste part en PCM.
/// Protection désactivée par la zone : DSD.
#[tokio::test]
async fn volume_logiciel_sous_100_repli_en_pcm() {
    let b = banc(true, "dop").await;
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    b.orch.playback.set_volume(b.zone_id, 0.5).await;
    let (mime, _, piste) = jouer(&b).await;
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)),
        Some(EtatModulation::Repli(MotifDsdRefuse::SoftwareVolumeActive))
    );
    let _ = mime;
    SettingsRepo::with_backend(b.orch.db.clone())
        .set(
            &crate::audio::dop_ecarte_7265::cle_protection_du_niveau(b.zone_id),
            "false",
        )
        .unwrap();
    let (_, _, piste) = jouer(&b).await;
    assert!(matches!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)),
        Some(EtatModulation::EnService(_))
    ));
}

/// Sans Premium, l'étage n'est pas effectif : rien ne change (la résolution
/// le dit `premium_required`), aucun état DSD n'est publié.
#[tokio::test]
async fn sans_premium_rien_ne_change() {
    let b = banc(false, "dop").await;
    ecrire_chaine(&b, None);
    let (_, reference, _) = jouer(&b).await;
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let (_, octets, piste) = jouer(&b).await;
    assert!(octets == reference);
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(piste)),
        None
    );
}

/// Les capacités publiées d'une zone locale « dop » sans plafond : les trois
/// cibles en DoP ; une zone plafonnée à 192 kHz refuse DSD128 et DSD256.
#[tokio::test]
async fn capacites_de_la_zone() {
    let b = banc(true, "dop").await;
    let c = b.orch.capacites_dsd_de_zone(b.zone_id).await;
    assert!(c.output_local);
    for t in &c.targets {
        assert!(t.famille_44k1.accepted, "{:?}", t.rate);
        assert_eq!(t.famille_44k1.transport, Some(TransportModulation::Dop));
    }
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_max_sample_rate(b.zone_id, Some(192_000))
        .unwrap();
    let c = b.orch.capacites_dsd_de_zone(b.zone_id).await;
    assert!(c.cible(TauxDsd::Dsd64).unwrap().famille_44k1.accepted);
    assert_eq!(
        c.cible(TauxDsd::Dsd128).unwrap().famille_44k1.reason,
        Some(MotifDsdRefuse::RateAboveZoneMax)
    );
}
