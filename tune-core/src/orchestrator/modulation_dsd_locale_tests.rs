//! rc14 — TESS vers le DSD sur la sortie locale, de la chaîne de la zone aux
//! octets de la session : non-régression du PCM (comparé à la SOURCE, pas à
//! un autre passage de la PR), porteur DoP de bout en bout, mono porté en
//! stéréo, multicanal en repli, volume avant le modulateur, headroom et
//! crêtes intersample par la chaîne complète.

use super::*;
use std::sync::Arc;
use std::time::Duration;

use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::tess::chaine::{ChaineTess, EtagesFixes, Headroom, TauxDsd, Upsampling};
use crate::tess::modulation_dsd::{EtatModulation, MotifDsdRefuse, TransportModulation};

const FLAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_16_44100_stereo.flac"
);
const FLAC_MONO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_16_44100_mono.flac"
);

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    pistes: std::sync::atomic::AtomicI64,
    /// Le registre de `tess::modulation_dsd` est global, et chaque banc a sa
    /// base neuve : les bancs passent un par un.
    _tour: tokio::sync::MutexGuard<'static, ()>,
}

static TOUR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Zone et pistes du banc : loin des identifiants des autres bancs, que
/// toute résolution locale fait passer par le registre.
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
        // Sans compte Premium : la licence existe et dit Free.
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
    // La machine de test (debug) n'est pas celle de la production.
    crate::tess::modulation_dsd::retenir_mesure_de_la_machine([50.0, 25.0, 12.0]);
    let b = Banc {
        orch,
        zone_id,
        pistes: std::sync::atomic::AtomicI64::new(0),
        _tour: tour,
    };
    // rc14.1 — le DAC du banc est tenu SEUL (le DoP modulé ne part que là).
    sortie_exclusive(&b, true);
    b
}

/// rc14.1 (fils 3123, 3126) — la sortie du banc, exclusive ou partagée, sur
/// toutes les plateformes : la case « exclusif » (macOS, Windows) et le PCM
/// enregistré de la sortie (Linux : `hw:` ou le mixeur `default`).
fn sortie_exclusive(b: &Banc, exclusive: bool) {
    SettingsRepo::with_backend(b.orch.db.clone())
        .set(
            "local_exclusive_mode",
            if exclusive { "true" } else { "false" },
        )
        .unwrap();
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_output_endpoint_id(
            b.zone_id,
            if exclusive {
                "alsa:hw:CARD=DSD,DEV=0"
            } else {
                "alsa:default"
            },
        )
        .unwrap();
}

/// Une piste de la bibliothèque, à un identifiant propre au banc.
fn piste(b: &Banc, chemin: &str, format: &str, canaux: i32) -> i64 {
    let mut t = Track::new("Piste du banc DSD rc14".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some(format.into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = canaux;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let id = TrackRepo::with_backend(b.orch.db.clone())
        .create(&t)
        .unwrap();
    let voulu = ID_DU_BANC + b.pistes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    b.orch
        .db
        .execute("UPDATE tracks SET id = ? WHERE id = ?", &[&voulu, &id])
        .unwrap();
    voulu
}

fn chaine(b: &Banc, upsampling: Option<Upsampling>, headroom: Headroom) {
    let s = SettingsRepo::with_backend(b.orch.db.clone());
    match upsampling {
        None => {
            let _ = s.delete(&crate::tess::cle_chaine_zone(b.zone_id));
        }
        Some(u) => {
            let c = ChaineTess {
                fixed: EtagesFixes {
                    upsampling: u,
                    headroom,
                    ..EtagesFixes::default()
                },
                ..ChaineTess::default()
            };
            crate::tess::migration::ecrire_chaine(&s, b.zone_id, &c).unwrap();
        }
    }
}

fn ecrire_chaine(b: &Banc, upsampling: Option<Upsampling>) {
    chaine(b, upsampling, Headroom::Auto {});
}

struct Joue {
    mime: String,
    octets: Vec<u8>,
    track_id: i64,
    stream_id: String,
}

impl Joue {
    fn etat(&self, b: &Banc) -> Option<EtatModulation> {
        crate::tess::modulation_dsd::etat_pour(
            b.zone_id,
            Some(&self.stream_id),
            Some(self.track_id),
        )
    }
}

/// Résout la piste et rend tous les octets de sa session.
async fn jouer(b: &Banc, track_id: i64) -> Joue {
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
    Joue {
        mime: r.mime_type,
        octets,
        track_id,
        stream_id: sid,
    }
}

fn dsd(rate: TauxDsd) -> Upsampling {
    Upsampling::Dsd {
        rate,
        filter: crate::audio::surechantillonnage::greffon::DEFAULT_FILTER.into(),
    }
}

/// Le PCM d'un WAV servi (bloc `data`), cadré à gauche sur 32 bits, avec
/// sa cadence et ses voies.
fn pcm_du_wav(octets: &[u8]) -> (u32, u16, Vec<i32>) {
    let voies = u16::from_le_bytes([octets[22], octets[23]]);
    let cadence = u32::from_le_bytes(octets[24..28].try_into().unwrap());
    let bits = u16::from_le_bytes([octets[34], octets[35]]);
    let mut debut = 12;
    while &octets[debut..debut + 4] != b"data" {
        let n = u32::from_le_bytes(octets[debut + 4..debut + 8].try_into().unwrap()) as usize;
        debut += 8 + n + n % 2;
    }
    let data = &octets[debut + 8..];
    let mots = match bits {
        16 => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| i32::from(i16::from_le_bytes(*s)) << 16)
            .collect(),
        24 => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|s| i32::from_le_bytes([0, s[0], s[1], s[2]]))
            .collect(),
        _ => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|s| i32::from_le_bytes(*s))
            .collect(),
    };
    (cadence, voies, mots)
}

/// LE témoin de non-régression, contre la SOURCE : avec `pcm`, `off` ou sans
/// chaîne, la zone sert le fichier lui-même (passthrough) ou son PCM exact —
/// le chemin PCM complet, tel qu'il était avant cette PR — alors même que
/// tout est réuni pour que le DSD s'engage. Contre-épreuve : `dsd`.
#[tokio::test]
async fn pcm_off_et_sans_chaine_servent_la_source_a_l_octet_pres() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    let source = std::fs::read(FLAC).unwrap();
    // La référence indépendante de la PR : le fichier lui-même
    // (passthrough), ou son PCM décodé, cadré à gauche sur 32 bits.
    let decode = crate::audio::decode::decode_to_pcm(FLAC, None, None, 0.0, 0.0).unwrap();
    let decalage = 32 - u32::from(decode.bit_depth);
    let attendu: Vec<i32> = decode.samples_i32.iter().map(|s| s << decalage).collect();
    let conforme = |j: &Joue| {
        if j.octets.starts_with(b"RIFF") {
            let (cadence, voies, mots) = pcm_du_wav(&j.octets);
            cadence == 44_100 && voies == 2 && mots == attendu
        } else {
            j.octets == source
        }
    };
    for u in [
        None,
        Some(Upsampling::Off {}),
        Some(Upsampling::Pcm {
            filter: crate::audio::surechantillonnage::greffon::DEFAULT_FILTER.into(),
            max_ratio: Some(4),
            max_rate_hz: None,
        }),
    ] {
        ecrire_chaine(&b, u.clone());
        let j = jouer(&b, id).await;
        assert!(
            conforme(&j),
            "{u:?} : le flux PCM n'est plus la source ({})",
            j.mime
        );
        assert_eq!(j.etat(&b), None, "{u:?}");
    }
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let j = jouer(&b, id).await;
    assert!(!conforme(&j), "contre-épreuve : le DSD change le flux");
}

fn verifier_dop_stereo(j: &Joue, cadence: u32) -> &[u8] {
    assert_eq!(j.mime, "audio/wav");
    let o = &j.octets;
    assert_eq!(&o[..4], b"RIFF");
    let c = u32::from_le_bytes(o[24..28].try_into().unwrap());
    let voies = u16::from_le_bytes(o[22..24].try_into().unwrap());
    let bits = u16::from_le_bytes(o[34..36].try_into().unwrap());
    assert_eq!((c, voies, bits), (cadence, 2, 24));
    let charge = &o[44..];
    assert_eq!(charge.len() % 12, 0, "trames DoP paires");
    let mut attendu = 0x05u8;
    for t in charge.as_chunks::<6>().0 {
        assert_eq!((t[2], t[5]), (attendu, attendu));
        attendu = if attendu == 0x05 { 0xFA } else { 0x05 };
    }
    #[cfg(feature = "local-audio")]
    assert!(crate::outputs::local::is_dop_pcm(charge, 24, 2));
    charge
}

#[tokio::test]
async fn une_piste_pcm_part_en_dop_dsd64_de_bout_en_bout() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let j = jouer(&b, id).await;
    let charge = verifier_dop_stereo(&j, 176_400);
    let trames_pcm = crate::audio::decode::decode_to_pcm(FLAC, None, None, 0.0, 0.0)
        .unwrap()
        .samples_i32
        .len()
        / 2;
    let trames = charge.len() / 6;
    // 4 trames DoP par trame PCM ; la queue du filtre (≈ 1 ms) attend une
    // éventuelle piste suivante.
    assert!(
        trames <= trames_pcm * 4 && trames + 400 >= trames_pcm * 4,
        "{trames}"
    );
    match j.etat(&b) {
        Some(EtatModulation::EnService(m)) => {
            assert_eq!(m.transport, TransportModulation::Dop);
            assert_eq!(
                (m.rate_hz, m.carrier_rate_hz, m.order),
                (2_822_400, 176_400, 7)
            );
            assert_eq!(
                m.headroom_db, -3.0,
                "headroom automatique avant le modulateur"
            );
        }
        autre => panic!("état {autre:?}"),
    }
}

/// HAUTE de la relecture : une source MONO part en DoP STÉRÉO (un DoP mono
/// serait coupé par la sortie, qui ouvre deux voies).
#[tokio::test]
async fn une_source_mono_part_en_dop_stereo() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC_MONO, "flac", 1);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let j = jouer(&b, id).await;
    let charge = verifier_dop_stereo(&j, 176_400);
    // La même voie des deux côtés (le dither diffère : même niveau, pas les
    // mêmes bits).
    let (g, d) = (rms_porte(charge, 0), rms_porte(charge, 1));
    assert!(g > 0.01 && (g - d).abs() < 0.1 * g, "voies {g} / {d}");
    match j.etat(&b) {
        Some(EtatModulation::EnService(m)) => assert_eq!((m.source_channels, m.channels), (1, 2)),
        autre => panic!("état {autre:?}"),
    }
}

/// Un 5.1 : le porteur est stéréo, la piste repart en PCM, motif dit.
#[tokio::test]
async fn une_source_multicanal_repart_en_pcm() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 6);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let j = jouer(&b, id).await;
    assert!(
        !j.octets.starts_with(b"RIFF") || u16::from_le_bytes([j.octets[34], j.octets[35]]) != 24
    );
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, None, Some(id)),
        Some(EtatModulation::Repli(MotifDsdRefuse::ChannelsNotSupported))
    );
}

/// Une zone qui n'a pas déclaré son DAC DSD (`auto`) : rien en DoP, la piste
/// part en PCM comme avant, et le motif est retenu.
#[tokio::test]
async fn zone_auto_repli_en_pcm_dit() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    ecrire_chaine(&b, None);
    let reference = jouer(&b, id).await.octets;
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_dsd_mode(b.zone_id, "auto")
        .unwrap();
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd128)));
    let j = jouer(&b, id).await;
    assert!(j.octets == reference, "le repli sert le PCM d'avant");
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(&j.stream_id), Some(id)),
        Some(EtatModulation::Repli(MotifDsdRefuse::DsdModeNotEnabled))
    );
}

/// Énergie (RMS) du signal porté, par fenêtres de 64 octets DSD d'une voie.
fn rms_porte(charge: &[u8], voie: usize) -> f64 {
    let dsd: Vec<u8> = charge
        .as_chunks::<6>()
        .0
        .iter()
        .flat_map(|t| [t[3 * voie + 1], t[3 * voie]])
        .collect();
    let moyennes: Vec<f64> = dsd
        .chunks(64)
        .map(|f| {
            let uns: u32 = f.iter().map(|o| o.count_ones()).sum();
            (2.0 * f64::from(uns) - 8.0 * f.len() as f64) / (8.0 * f.len() as f64)
        })
        .collect();
    (moyennes.iter().map(|m| m * m).sum::<f64>() / moyennes.len() as f64).sqrt()
}

/// Q9 : le volume de la zone s'applique juste avant le modulateur. À 50 %,
/// la piste reste modulée (aucun repli, aucune relance) et porte la moitié
/// du niveau ; en sourdine, rien ; `dsd_volume_protection` n'y change rien.
#[tokio::test]
async fn le_volume_s_applique_avant_le_modulateur() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let plein = rms_porte(verifier_dop_stereo(&jouer(&b, id).await, 176_400), 0);
    b.orch.playback.set_volume(b.zone_id, 0.5).await;
    let j = jouer(&b, id).await;
    assert!(matches!(j.etat(&b), Some(EtatModulation::EnService(_))));
    let moitie = rms_porte(verifier_dop_stereo(&j, 176_400), 0);
    let rapport = moitie / plein;
    assert!((0.4..0.6).contains(&rapport), "rapport {rapport}");
}

/// Point 10 de la relecture : un master chaud (crête intersample à +3 dB)
/// par la CHAÎNE COMPLÈTE (décodage, greffons, headroom, volume,
/// modulateur). La marge propre du modulateur (−3 dB) s'ajoute, une fois,
/// au headroom de la chaîne : automatique, ou manuel à 0 dB, le limiteur
/// n'est jamais atteint ; manuel à −6 dB, la marge est de 9 dB.
/// (La contre-épreuve, sans marge, est celle du producteur :
/// `le_headroom_garde_les_cretes_intersample_sous_le_limiteur`.)
#[tokio::test]
async fn crete_intersample_par_la_chaine_complete() {
    let b = banc(true, "dop").await;
    let wav = crate::test_scratch::scratch_file("intersample-rc14", ".wav");
    let trames = 44_100usize;
    let mut w =
        crate::audio::wav::build_wav_header_with_data_size(2, 44_100, 16, (trames * 4) as u32)
            .to_vec();
    for i in 0..trames {
        let v = [32_767i16, 32_767, -32_767, -32_767][i % 4];
        w.extend_from_slice(&v.to_le_bytes());
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(wav.path(), &w).unwrap();
    let chemin = wav.path().to_string_lossy().to_string();
    let id = piste(&b, &chemin, "wav", 2);
    for (headroom, marge) in [
        (Headroom::Auto {}, -3.0),
        (Headroom::Manual { db: 0.0 }, -3.0),
        (Headroom::Manual { db: -6.0 }, -9.0),
    ] {
        chaine(&b, Some(dsd(TauxDsd::Dsd64)), headroom);
        let j = jouer(&b, id).await;
        verifier_dop_stereo(&j, 176_400);
        let mut bilan = None;
        for _ in 0..100 {
            bilan = crate::tess::modulation_dsd::bilan(&j.stream_id);
            if bilan.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let bilan = bilan.expect("bilan du flux");
        assert_eq!(bilan.reinitialisations, 0, "{headroom:?}");
        assert_eq!(bilan.ecretages, 0, "{headroom:?} : {bilan:?}");
        match j.etat(&b) {
            Some(EtatModulation::EnService(m)) => assert_eq!(m.headroom_db, marge, "{headroom:?}"),
            autre => panic!("état {autre:?}"),
        }
    }
}

/// Sans Premium, l'étage n'est pas effectif : rien ne change.
#[tokio::test]
async fn sans_premium_rien_ne_change() {
    let b = banc(false, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    ecrire_chaine(&b, None);
    let reference = jouer(&b, id).await.octets;
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let j = jouer(&b, id).await;
    assert!(j.octets == reference);
    assert_eq!(j.etat(&b), None);
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

/// v4 D2 : les sentinelles de longueur inconnue ne bornent pas le flux (une
/// longue piste de durée inconnue n'est jamais coupée) ; une vraie longueur
/// le borne.
#[test]
fn les_sentinelles_de_longueur_ne_bornent_rien() {
    for sentinelle in [u32::MAX, 0x7FFF_FFFF - 36, 0] {
        let e = crate::audio::wav::build_wav_header_with_data_size(2, 176_400, 24, sentinelle);
        assert_eq!(super::longueur_annoncee(&e), None, "{sentinelle:#x}");
    }
    let e = crate::audio::wav::build_wav_header_with_data_size(2, 176_400, 24, 1_200);
    assert_eq!(super::longueur_annoncee(&e), Some(1_200));
    assert_eq!(super::longueur_annoncee(b"TUNEDSD1________"), None);
}

/// Niveau (dBFS PCM) porté par une voie d'un flux DoP : RMS de la densité
/// DSD par fenêtres de 256 octets (≈ 0,7 ms à DSD64), le premier et le
/// dernier dixième écartés, rapporté au RMS d'un sinus à 0 dBFS (modulation
/// de 50 %, le « 0 dB SACD »).
fn niveau_porte_dbfs(charge: &[u8], voie: usize) -> f64 {
    let dsd: Vec<u8> = charge
        .as_chunks::<6>()
        .0
        .iter()
        .flat_map(|t| [t[3 * voie + 1], t[3 * voie]])
        .collect();
    let moyennes: Vec<f64> = dsd
        .chunks_exact(256)
        .map(|f| {
            let uns: u32 = f.iter().map(|o| o.count_ones()).sum();
            (2.0 * f64::from(uns) - 8.0 * f.len() as f64) / (8.0 * f.len() as f64)
        })
        .collect();
    let n = moyennes.len();
    let utiles = &moyennes[n / 10..n - n / 10];
    let rms = (utiles.iter().map(|m| m * m).sum::<f64>() / utiles.len() as f64).sqrt();
    let reference = crate::tess::modulation_dsd::MODULATION_A_0_DBFS / std::f64::consts::SQRT_2;
    20.0 * (rms / reference).log10()
}

/// Revue rc14 (#9518 × #9520) : la marge manuelle de TESS ne s'applique
/// qu'UNE fois sous modulation DSD, et la marge propre du modulateur
/// (−3 dB) s'y ajoute une seule fois. Mesuré sur les octets servis (sinus
/// de 44,1 Hz à 0 dBFS), sans puis avec un égaliseur coché : le niveau
/// porté est celui que publie le chemin du signal, et le modulateur ne
/// ramène jamais rien au limiteur.
#[tokio::test]
async fn la_marge_manuelle_ne_s_applique_qu_une_fois_sous_modulation() {
    use crate::tess::chaine::MaillonGreffon;
    use crate::tess::modulation_dsd::MARGE_DU_MODULATEUR_DB;
    let b = banc(true, "dop").await;
    let s = SettingsRepo::with_backend(b.orch.db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    let wav = crate::test_scratch::scratch_file("marge-une-fois-rc14", ".wav");
    let trames = 44_100usize;
    let mut w =
        crate::audio::wav::build_wav_header_with_data_size(2, 44_100, 16, (trames * 4) as u32)
            .to_vec();
    for i in 0..trames {
        let x = (2.0 * std::f64::consts::PI * i as f64 / 1_000.0).sin();
        let v = (x * 32_767.0).round() as i16;
        w.extend_from_slice(&v.to_le_bytes());
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(wav.path(), &w).unwrap();
    let chemin = wav.path().to_string_lossy().to_string();
    let id = piste(&b, &chemin, "wav", 2);
    // Un paramétrique qui ne touche pas 44,1 Hz : une coupe à 12 kHz.
    let egaliseur = MaillonGreffon {
        id: crate::tess::migration::ID_EQ_PARAMETRIQUE.into(),
        enabled: true,
        settings: {
            let p = crate::audio::eq::EqProfile {
                enabled: true,
                bands: vec![crate::audio::eq::EqBandSpec {
                    freq: 12_000.0,
                    gain: -3.0,
                    q: 1.0,
                    band_type: "peak".into(),
                    channel: None,
                }],
                ..Default::default()
            };
            let mut v = serde_json::to_value(p).unwrap();
            v.as_object_mut().unwrap().remove("enabled");
            v
        },
    };
    let mut mesures = Vec::new();
    for (headroom, avec_eq, attendu) in [
        (Headroom::Auto {}, false, MARGE_DU_MODULATEUR_DB),
        (Headroom::Manual { db: 0.0 }, false, MARGE_DU_MODULATEUR_DB),
        (
            Headroom::Manual { db: -12.0 },
            false,
            -12.0 + MARGE_DU_MODULATEUR_DB,
        ),
        (
            Headroom::Manual { db: -12.0 },
            true,
            -12.0 + MARGE_DU_MODULATEUR_DB,
        ),
        (
            Headroom::Manual { db: -2.0 },
            true,
            -2.0 + MARGE_DU_MODULATEUR_DB,
        ),
    ] {
        let c = ChaineTess {
            plugins: if avec_eq {
                vec![egaliseur.clone()]
            } else {
                Vec::new()
            },
            fixed: EtagesFixes {
                upsampling: dsd(TauxDsd::Dsd64),
                headroom,
                ..EtagesFixes::default()
            },
            ..ChaineTess::default()
        };
        crate::tess::migration::ecrire_chaine(&s, b.zone_id, &c).unwrap();
        let j = jouer(&b, id).await;
        let mesure = niveau_porte_dbfs(verifier_dop_stereo(&j, 176_400), 0);
        let mut bilan = None;
        for _ in 0..100 {
            bilan = crate::tess::modulation_dsd::bilan(&j.stream_id);
            if bilan.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let bilan = bilan.expect("bilan du flux");
        let publie = match j.etat(&b) {
            Some(EtatModulation::EnService(m)) => m.headroom_db,
            autre => panic!("état {autre:?}"),
        };
        mesures.push((headroom, avec_eq, attendu, mesure, publie, bilan));
    }
    for (headroom, avec_eq, attendu, mesure, publie, bilan) in &mesures {
        eprintln!(
            "marge {headroom:?} égaliseur={avec_eq} : mesuré {mesure:.2} dBFS, publié {publie} dB, \
             attendu {attendu} dB, entrée max {:.3}, écrêtages {}",
            bilan.entree_max, bilan.ecretages
        );
    }
    for (headroom, avec_eq, attendu, mesure, publie, bilan) in &mesures {
        let quoi = format!("{headroom:?} égaliseur={avec_eq}");
        assert!(
            (mesure - attendu).abs() < 0.5,
            "{quoi} : mesuré {mesure:.2} dBFS"
        );
        assert_eq!(publie, attendu, "{quoi} : chemin du signal");
        assert_eq!(bilan.reinitialisations, 0, "{quoi}");
        assert_eq!(bilan.ecretages, 0, "{quoi} : {bilan:?}");
    }
}

/// Relecture de #9575 (sécurité auditive) — une piste modulée n'est relancée
/// que si elle RESTERAIT modulée : une relance qui repartirait en PCM perdrait
/// la marge du modulateur (−3 dB) en cours de piste. Cible éteinte ou PCM,
/// DSD256 que la machine ne tient pas : pas de relance (piste suivante).
/// Contre-épreuve : DSD64 → DSD128, que la sortie et la machine prennent,
/// relance.
#[tokio::test]
async fn une_relance_qui_repartirait_en_pcm_n_a_pas_lieu() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    let j = jouer(&b, id).await;
    let Some(EtatModulation::EnService(m)) = j.etat(&b) else {
        panic!("piste non modulée : {:?}", j.etat(&b));
    };
    assert_eq!(m.headroom_db, -3.0, "la marge que le PCM n'aurait pas");

    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd128)));
    assert!(b.orch.la_relance_resterait_modulee(b.zone_id, &m).await);

    ecrire_chaine(&b, Some(Upsampling::Off {}));
    assert!(!b.orch.la_relance_resterait_modulee(b.zone_id, &m).await);
    ecrire_chaine(
        &b,
        Some(Upsampling::Pcm {
            filter: crate::audio::surechantillonnage::greffon::DEFAULT_FILTER.into(),
            max_ratio: None,
            max_rate_hz: None,
        }),
    );
    assert!(!b.orch.la_relance_resterait_modulee(b.zone_id, &m).await);

    // DSD256 refusé (machine trop lente) : le repli serait en PCM.
    crate::tess::modulation_dsd::retenir_mesure_de_la_machine([50.0, 25.0, 0.2]);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd256)));
    let refusee = b.orch.la_relance_resterait_modulee(b.zone_id, &m).await;
    crate::tess::modulation_dsd::retenir_mesure_de_la_machine([50.0, 25.0, 12.0]);
    assert!(!refusee);
    assert!(b.orch.la_relance_resterait_modulee(b.zone_id, &m).await);
}

/// rc14.1 (fils 3123, 3126) — la règle de l'exclusivité, plateforme par
/// plateforme, contre-épreuves comprises.
#[test]
fn seule_une_sortie_exclusive_peut_porter_le_dop_module() {
    use super::sortie_locale_exclusive as excl;
    // macOS : CoreAudio partagé (« Exclusive mode requested=false ») / hog.
    assert!(!excl("macos", false, false, "auto", None));
    assert!(excl("macos", false, true, "auto", None));
    // Windows : WASAPI partagé / exclusif ; ASIO non compilé retombe sur le
    // partagé, ASIO compilé est exclusif.
    assert!(!excl("windows", true, false, "wasapi", None));
    assert!(excl("windows", false, true, "wasapi", None));
    assert!(!excl("windows", false, true, "asio", None));
    assert!(excl("windows", true, true, "asio", None));
    // Linux : `hw:` seul ; le mixeur et `plughw:` (conversion muette) non.
    assert!(excl(
        "linux",
        false,
        false,
        "auto",
        Some("alsa:hw:CARD=E30,DEV=0")
    ));
    assert!(excl(
        "linux",
        false,
        false,
        "auto",
        Some("hw:CARD=E30,DEV=0")
    ));
    for partage in [
        "alsa:plughw:CARD=E30,DEV=0",
        "alsa:default",
        "alsa:default:CARD=E30",
        "alsa:sysdefault:CARD=E30",
        "alsa:dmix:CARD=E30,DEV=0",
        "alsa:pipewire",
        "alsa:pulse",
    ] {
        assert!(
            !excl("linux", false, true, "auto", Some(partage)),
            "{partage}"
        );
    }
    assert!(
        !excl("linux", false, true, "auto", None),
        "endpoint inconnu"
    );
}

/// rc14.1 (fils 3123, 3126) — sortie PARTAGÉE : pas de DoP modulé, la piste
/// sert le PCM d'avant à l'octet près (aucun saut de niveau), le motif
/// `shared_output` est retenu et publié ; la relance d'une piste modulée
/// n'aurait pas lieu. Contre-épreuve : la même sortie en exclusif module.
#[tokio::test]
async fn une_sortie_partagee_ne_porte_pas_de_dop_module() {
    let b = banc(true, "dop").await;
    let id = piste(&b, FLAC, "flac", 2);
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    // Contre-épreuve d'abord : exclusive, la piste part en DoP.
    let module = jouer(&b, id).await;
    let Some(EtatModulation::EnService(m)) = module.etat(&b) else {
        panic!("piste non modulée en exclusif : {:?}", module.etat(&b));
    };
    assert!(b.orch.la_relance_resterait_modulee(b.zone_id, &m).await);
    // La référence : le PCM d'avant, sans chaîne vers le DSD.
    ecrire_chaine(&b, None);
    let reference = jouer(&b, id).await.octets;
    ecrire_chaine(&b, Some(dsd(TauxDsd::Dsd64)));
    sortie_exclusive(&b, false);
    assert!(
        !b.orch.la_relance_resterait_modulee(b.zone_id, &m).await,
        "une relance sur la sortie partagée repartirait en PCM"
    );
    let j = jouer(&b, id).await;
    assert!(j.octets == reference, "le repli sert le PCM d'avant");
    assert_eq!(
        crate::tess::modulation_dsd::etat_pour(b.zone_id, Some(&j.stream_id), Some(id)),
        Some(EtatModulation::Repli(MotifDsdRefuse::SharedOutput))
    );
    let c = b.orch.capacites_dsd_de_zone(b.zone_id).await;
    for t in &c.targets {
        assert!(!t.famille_44k1.accepted, "{:?}", t.rate);
        assert_eq!(t.famille_44k1.reason, Some(MotifDsdRefuse::SharedOutput));
    }
    // Retour en exclusif : la modulation revient.
    sortie_exclusive(&b, true);
    let j = jouer(&b, id).await;
    verifier_dop_stereo(&j, 176_400);
    assert!(matches!(j.etat(&b), Some(EtatModulation::EnService(_))));
}
