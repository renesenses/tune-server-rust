//! #7986 — un FLAC 16/44,1 vers une zone DLNA qui accepte audio/flac, sans
//! traitement qui s'entende, part TEL QUEL : pas de fichier temporaire, pas
//! d'entrée de cache, la session sert le fichier d'origine.
//!
//! Le journal du testeur montrait `transcode_required source=Flac
//! target=Flac` suivi d'un transcodage `convolution=false egaliseur=false
//! replaygain_db=None` : un motif était vrai alors que rien n'était appliqué.
//! Le seul motif commun au FLAC et au WAV sur une bibliothèque ordinaire est
//! `eq_forces_transcode` par la correction de pièce : une réponse
//! impulsionnelle enregistrée, présente sur le disque, mais que le convolveur
//! refuse au moment de transcoder (cadence ou canaux différents, fichier
//! illisible, `room_correction_ir_load_failed`).

use super::*;
use std::sync::Arc;

use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;

const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");

/// Un orchestrateur Premium (le droit `RoomCorrection` est requis pour que la
/// réponse impulsionnelle compte), une zone DLNA dont le renderer est inconnu
/// du registre : réputé accepter tout MIME, donc audio/flac.
fn banc() -> (PlaybackOrchestrator, i64) {
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
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));
    let zone_id = ZoneRepo::with_backend(db)
        .create(
            "Salon DLNA (banc 7986)",
            Some("dlna"),
            Some("uuid:renderer-flac"),
        )
        .unwrap();
    (orch, zone_id)
}

/// Une réponse impulsionnelle WAV float 32, Dirac de gain 0,5.
fn ecrire_ri(nom: &str, cadence: u32, canaux: u16) -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file(nom, ".wav");
    let taps = 32usize;
    let donnees = taps * 4 * usize::from(canaux);
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&canaux.to_le_bytes());
    w.extend_from_slice(&cadence.to_le_bytes());
    w.extend_from_slice(&(cadence * 4 * u32::from(canaux)).to_le_bytes());
    w.extend_from_slice(&(4 * canaux).to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        for _ in 0..canaux {
            let v: f32 = if i == 0 { 0.5 } else { 0.0 };
            w.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(f.path(), w).unwrap();
    f
}

fn regler_ri(orch: &PlaybackOrchestrator, zone_id: i64, chemin: &str) {
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("ir_path_{zone_id}"), chemin)
        .unwrap();
}

/// Résout la piste FLAC 16/44,1 de la caisse sur la zone, et rend l'élément
/// résolu avec le chemin que sa session sert.
async fn resoudre_le_flac(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
) -> (crate::orchestrator::ResolvedQueueItem, Option<String>) {
    let mut t = Track::new("Piste du banc 7986".into());
    t.duration_ms = 1_000;
    t.file_path = Some(FLAC.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(FLAC).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = TrackRepo::with_backend(orch.db.clone()).create(&t).unwrap();
    PlayQueueRepo::with_backend(orch.db.clone())
        .append(zone_id, &[QueueInput::Local { track_id }])
        .unwrap();
    let r = orch
        .resolve_queue_item_url(zone_id, 0)
        .await
        .expect("résolution");
    let sid = r.stream_id.clone().expect("une session est ouverte");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session vivante");
    let servi = session.file_path.lock().await.clone();
    (r, servi)
}

/// Le TÉMOIN de #7986. Une RI 48 kHz est enregistrée et présente sur le
/// disque, mais le flux est à 44,1 kHz : le convolveur la refusera
/// (`ConvolverConfig::build_for`). Avant le correctif, sa seule existence
/// allumait `eq_forces_transcode` : FLAC décodé, ré-encodé à l'identique dans
/// un fichier temporaire puis mis en cache, `convolution=false`.
#[tokio::test]
async fn un_flac_16_44_vers_une_zone_dlna_flac_sans_traitement_applicable_part_tel_quel_7986() {
    let (orch, zone_id) = banc();
    let ri = ecrire_ri("ri-7986-48k", 48_000, 1);
    regler_ri(&orch, zone_id, ri.as_str().as_ref());
    assert!(
        orch.zone_has_active_ir(zone_id),
        "prémisse : la RI est réglée, sous licence, et le fichier existe"
    );

    let (r, servi) = resoudre_le_flac(&orch, zone_id).await;

    assert_eq!(r.mime_type, "audio/flac");
    assert_eq!(
        servi.as_deref(),
        Some(FLAC),
        "#7986 : la session doit servir le FLAC d'origine, pas un fichier \
         transcodé ({servi:?}) — aucune RI ne s'applique à 44,1 kHz, le \
         transcodage n'aurait rien changé au son"
    );
    assert_eq!(
        r.file_size,
        std::fs::metadata(FLAC).ok().map(|m| m.len()),
        "#7986 : en passthrough, la taille annoncée est celle du fichier"
    );
}

/// Même refus pour un fichier de RI illisible (vide) : il existe, mais rien
/// ne se chargera.
#[tokio::test]
async fn une_ri_illisible_ne_force_pas_le_transcodage_7986() {
    let (orch, zone_id) = banc();
    let ri = crate::test_scratch::scratch_file("ri-7986-vide", ".wav");
    std::fs::write(ri.path(), b"").unwrap();
    regler_ri(&orch, zone_id, ri.as_str().as_ref());

    let (_, servi) = resoudre_le_flac(&orch, zone_id).await;

    assert_eq!(servi.as_deref(), Some(FLAC), "{servi:?}");
}

/// La garde ne désarme pas une correction de pièce qui s'applique : une RI à
/// la cadence du flux garde le transcodage, et donc la convolution.
#[tokio::test]
async fn une_ri_a_la_cadence_du_flux_garde_le_transcodage_7986() {
    let (orch, zone_id) = banc();
    let ri = ecrire_ri("ri-7986-44k", 44_100, 1);
    regler_ri(&orch, zone_id, ri.as_str().as_ref());

    let (_, servi) = resoudre_le_flac(&orch, zone_id).await;

    assert_ne!(
        servi.as_deref(),
        Some(FLAC),
        "une RI applicable doit être cuite dans le flux : le fichier d'origine \
         ne peut pas partir tel quel"
    );
}

/// Les refus de `zone_ir_s_appliquerait` sont ceux de `build_for`.
#[test]
fn la_ri_ne_s_applique_qu_a_sa_cadence_et_a_un_nombre_de_canaux_compatible_7986() {
    let (orch, zone_id) = banc();
    assert!(
        !orch.zone_ir_s_appliquerait(zone_id, Some(44_100), 2),
        "sans RI"
    );

    let mono = ecrire_ri("ri-7986-mono", 44_100, 1);
    regler_ri(&orch, zone_id, mono.as_str().as_ref());
    assert!(
        orch.zone_ir_s_appliquerait(zone_id, Some(44_100), 2),
        "mono dupliquée"
    );
    assert!(
        orch.zone_ir_s_appliquerait(zone_id, None, 2),
        "cadence inconnue"
    );
    assert!(
        !orch.zone_ir_s_appliquerait(zone_id, Some(48_000), 2),
        "cadence"
    );

    let stereo = ecrire_ri("ri-7986-stereo", 44_100, 2);
    regler_ri(&orch, zone_id, stereo.as_str().as_ref());
    assert!(orch.zone_ir_s_appliquerait(zone_id, Some(44_100), 2));
    assert!(
        !orch.zone_ir_s_appliquerait(zone_id, Some(44_100), 6),
        "canaux"
    );
}
