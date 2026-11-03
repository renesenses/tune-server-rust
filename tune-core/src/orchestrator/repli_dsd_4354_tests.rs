//! 🔴 #4354 — un DSD128 décimé en PCM qui ne remplit pas son pré-tampon
//! retombe à 176,4 kHz au lieu de jouer haché (décision de Bertrand, 12/10).
//!
//! La machine lente est SIMULÉE : la session de flux est nourrie par un
//! producteur bridé, qui livre ~0,1× le temps réel d'un 352,8 kHz / 24 bits.
//! Le jumeau rapide prouve que la barrière, elle, laisse passer un flux qui
//! tient la cadence : sans lui, « pas de `Play` » pourrait venir d'un témoin
//! qui n'envoie jamais rien.
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::StreamInfo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator, ResolvedStream};
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

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

/// Une zone DLNA en `dsd_mode = pcm`, sous un identifiant propre au test : le
/// repli vit dans un registre global, partagé par les tests du binaire.
async fn zone_dlna(orch: &PlaybackOrchestrator, renderer: &str, zone_id: i64) -> i64 {
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let id = zones
        .create("DMP-A8", Some("dlna"), Some(renderer))
        .unwrap();
    orch.db
        .execute(
            "UPDATE zones SET id = ? WHERE id = ?",
            &[&zone_id as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )
        .unwrap();
    zones.update_dsd_mode(zone_id, "pcm").unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(renderer, "DMP-A8").with_type("dlna"),
    ));
    crate::audio::repli_dsd_4354::oublier(zone_id);
    zone_id
}

fn piste_dsd(orch: &PlaybackOrchestrator, id: i64, chemin: &str, cadence_dsd: u32) {
    orch.db
        .execute(
            "INSERT INTO tracks (id, title, file_path, format, duration_ms, sample_rate, \
             bit_depth, channels) VALUES (?, 'Andante', ?, 'dsf', 2000, ?, 1, 2)",
            &[
                &id as &dyn ToSqlValue,
                &chemin.to_string() as &dyn ToSqlValue,
                &i64::from(cadence_dsd) as &dyn ToSqlValue,
            ],
        )
        .unwrap();
}

fn demande(zone_id: i64, renderer: &str, track_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(renderer.into()),
        track_id: Some(track_id),
        source: Some("local".into()),
        ..Default::default()
    }
}

/// Le témoin de la barrière : une session WAV 352,8 kHz / 24 bits, nourrie
/// à `octets_par_s`, envoyée à un renderer DLNA. Rend ce que la barrière a
/// répondu et le nombre de `Play` reçus par le renderer.
async fn envoyer_un_dsd128_decime(
    zone_id: i64,
    renderer: &str,
    octets_par_s: usize,
) -> ((bool, Option<String>), usize) {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, renderer, zone_id).await;
    piste_dsd(&orch, 1, "/musique/Lisa Jacobs/01 - Andante.dsf", 5_644_800);

    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 352_800,
        bit_depth: 24,
        channels: 2,
        ..StreamInfo::default()
    };
    let (sid, tx, _ready) = orch.streamer.create_session(info, false, 256).await;
    // Le producteur bridé : des morceaux de 32 Kio, au débit demandé.
    const MORCEAU: usize = 32 * 1024;
    let pause = std::time::Duration::from_secs_f64(MORCEAU as f64 / octets_par_s as f64);
    let producteur = tokio::spawn(async move {
        for _ in 0..400 {
            if tx.send(vec![0u8; MORCEAU]).await.is_err() {
                break;
            }
            tokio::time::sleep(pause).await;
        }
    });

    let req = demande(zone_id, renderer, 1);
    let resolved = ResolvedStream {
        url: format!("http://127.0.0.1:8888/stream/{sid}.wav"),
        mime_type: "audio/wav".into(),
        title: "Andante".into(),
        artist: None,
        album: None,
        duration_ms: Some(614_800),
        source: "local".into(),
        cover_url: None,
        stream_id: Some(sid.clone()),
        file_size: None,
        sample_rate: Some(352_800),
        bit_depth: Some(24),
        channels: Some(2),
        origin_url: None,
        bitrate_kbps: None,
        format_source: None,
    };
    let habillage = super::transport::Habillage {
        album: None,
        cover_path: None,
    };
    let rendu = orch
        .envoyer_a_la_sortie(
            &req,
            &resolved,
            &habillage,
            false,
            std::time::Instant::now(),
            0,
        )
        .await;
    producteur.abort();

    let outputs = orch.outputs.lock().await;
    let out = outputs.get(renderer).unwrap();
    let guard = out.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    (rendu, mock.play_call_count().await)
}

/// 🔴 #4354 — machine lente simulée : 0,1× le temps réel. La barrière ne
/// lance PAS le renderer, pose le repli de la zone et demande de rejouer.
#[tokio::test]
async fn un_dsd128_trop_lent_pour_le_pre_tampon_pose_un_repli_a_176_4_4354() {
    let zone_id = 435401;
    // 352 800 × 3 × 2 = 2 116 800 octets/s requis ; on en livre ~10 %.
    let ((envoye, message), plays) =
        envoyer_un_dsd128_decime(zone_id, "dlna-4354-lent", 211_680).await;

    assert_eq!(
        plays, 0,
        "le pré-tampon n'est pas atteint : le renderer ne doit pas recevoir le flux \
         à 352,8 kHz, qui jouerait haché"
    );
    assert!(!envoye);
    assert_eq!(
        message.as_deref(),
        Some(crate::audio::repli_dsd_4354::REJOUER_AVEC_LE_REPLI),
        "la barrière doit demander de rejouer la piste avec le repli"
    );
    assert_eq!(
        crate::audio::repli_dsd_4354::repli_de_la_zone(zone_id),
        Some(crate::audio::repli_dsd_4354::RepliDsd {
            de_hz: 352_800,
            vers_hz: 176_400
        }),
        "le repli de la zone doit être posé, de 352,8 vers 176,4 kHz"
    );
    crate::audio::repli_dsd_4354::oublier(zone_id);
}

/// Le jumeau rapide : un producteur qui tient 2× le temps réel remplit la
/// barrière ; le flux part tel quel, sans repli.
#[tokio::test]
async fn un_dsd128_qui_tient_la_cadence_part_a_352_8_sans_repli_4354() {
    let zone_id = 435402;
    let ((envoye, message), plays) =
        envoyer_un_dsd128_decime(zone_id, "dlna-4354-rapide", 4_233_600 * 4).await;

    assert!(
        envoye,
        "un flux qui tient la cadence doit partir : {message:?}"
    );
    assert_eq!(plays, 1);
    assert_eq!(
        crate::audio::repli_dsd_4354::repli_de_la_zone(zone_id),
        None,
        "aucun repli quand le pré-tampon est atteint"
    );
}

/// Un `.dsf` DSD128 stéréo valide, `blocs_par_canal` super-blocs de 4096
/// octets par canal (motif 0x69 : un DSD au repos).
fn ecrire_dsf128(path: &std::path::Path, blocs_par_canal: usize) {
    const BLOC: usize = 4096;
    const CANAUX: usize = 2;
    let data = vec![0x69u8; blocs_par_canal * BLOC * CANAUX];
    let total_samples = (blocs_par_canal * BLOC * 8) as u64;
    let mut buf = Vec::with_capacity(92 + data.len());
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&(CANAUX as u32).to_le_bytes());
    buf.extend_from_slice(&5_644_800u32.to_le_bytes()); // DSD128
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&(BLOC as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(path, &buf).unwrap();
}

/// 🔴 #4354 — une fois le repli posé, la même piste DSD128 se résout à
/// 176,4 kHz ; sans repli, elle se résout à 352,8 kHz (témoin).
#[tokio::test]
async fn apres_le_repli_le_dsd128_se_resout_a_176_4_4354() {
    let zone_id = 435403;
    let renderer = "dlna-4354-resolution";
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, renderer, zone_id).await;
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("andante-4354.dsf");
    ecrire_dsf128(&chemin, 8);
    piste_dsd(&orch, 1, &chemin.to_string_lossy(), 5_644_800);

    let sans_repli = orch
        .resolve_stream(&demande(zone_id, renderer, 1))
        .await
        .expect("le DSD128 se résout");
    assert_eq!(
        sans_repli.sample_rate,
        Some(352_800),
        "témoin : sans repli, un DSD128 décimé sort à 352,8 kHz ({})",
        sans_repli.mime_type
    );
    if let Some(sid) = sans_repli.stream_id.as_deref() {
        orch.streamer.remove_session(sid).await;
    }

    crate::audio::repli_dsd_4354::enregistrer(
        zone_id,
        crate::audio::repli_dsd_4354::RepliDsd {
            de_hz: 352_800,
            vers_hz: 176_400,
        },
    );
    let avec_repli = orch
        .resolve_stream(&demande(zone_id, renderer, 1))
        .await
        .expect("le DSD128 se résout avec le repli");
    crate::audio::repli_dsd_4354::oublier(zone_id);
    assert_eq!(
        avec_repli.sample_rate,
        Some(176_400),
        "le repli posé, le DSD128 doit sortir à 176,4 kHz ({})",
        avec_repli.mime_type
    );
}

/// 🔴 #4354 — de bout en bout, par `play()`, sur un hôte lent SIMULÉ : la
/// lecture du `.dsf` est freinée (20 ms par bloc de 5,8 ms d'audio, ~0,3× le
/// temps réel), donc la barrière de 2 s n'est pas remplie en 4 s. La zone pose
/// son repli, la piste est rejouée une fois, et le renderer ne reçoit QU'UN
/// `Play`, celui du flux à 176,4 kHz. Sans le retrait de la garde anti-doublon,
/// ce second passage serait avalé.
#[tokio::test]
async fn par_play_la_piste_est_rejouee_une_fois_a_176_4_4354() {
    let zone_id = 435404;
    let renderer = "dlna-4354-bout-en-bout";
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, renderer, zone_id).await;
    // Le flux progressif (le chemin du .42), pas le fichier transcodé
    // d'avance, dont la barrière ne mesure rien.
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set("dsd_lpcm_stream", "true")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("andante-lent-4354.dsf");
    // ~2,3 s d'audio : le producteur vit plus longtemps que la barrière.
    ecrire_dsf128(&chemin, 400);
    let chemin = chemin.to_string_lossy().into_owned();
    crate::audio::repli_dsd_4354::freiner_pour_test(&chemin, std::time::Duration::from_millis(20));
    piste_dsd(&orch, 1, &chemin, 5_644_800);

    let resultat = orch
        .play(demande(zone_id, renderer, 1))
        .await
        .expect("la lecture aboutit");
    let repli = crate::audio::repli_dsd_4354::repli_de_la_zone(zone_id);
    crate::audio::repli_dsd_4354::oublier(zone_id);

    assert_eq!(
        repli,
        Some(crate::audio::repli_dsd_4354::RepliDsd {
            de_hz: 352_800,
            vers_hz: 176_400
        }),
        "la barrière manquée doit poser le repli de la zone"
    );
    assert!(resultat.output_sent, "{resultat:?}");
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(renderer).unwrap();
    let guard = out.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    assert_eq!(
        mock.play_call_count().await,
        1,
        "un seul Play : celui du flux de repli (ni zéro, ni le flux à 352,8 kHz)"
    );
    let url = mock.last_play_url().await.expect("une URL jouée");
    let sid = crate::http::streamer::extract_stream_id(url.rsplit('/').next().unwrap_or_default());
    let fil = orch
        .streamer
        .stream_output_wire(sid)
        .await
        .expect("la session du flux joué");
    assert_eq!(
        fil.sample_rate, 176_400,
        "le flux joué doit être celui de repli, à 176,4 kHz"
    );
}
