//! #5071 — la compensation de niveau (#4685) cuite dans le flux RÉSEAU.
//!
//! Témoins de bout en bout : une piste de la BIBLIOTHÈQUE (FLAC stéréo
//! fabriqué pour l'occasion, sinus 1 kHz) jouée vers une zone DLNA dont
//! l'égaliseur pousse une bande — donc réserve sa marge et baisse le niveau
//! moyen. La mesure passe par `resolve_local_track`, la vraie résolution, puis
//! relit le fichier que la session SERT au renderer, et le compare à
//! l'étalon « égaliseur seul » : ce que servait le code d'avant.
use super::{PlayRequest, PlaybackOrchestrator};
use crate::audio::compensation_reseau::{PLAFOND_DBFS, crete_relative};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use std::sync::Arc;

const SR: u32 = 44_100;
const SECONDES: u32 = 4;
const DEVICE: &str = "uuid:marantz-nd8006-5071";

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
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

/// PCM 16 bits stéréo : sinus 1 kHz d'amplitude `amplitude` sur les deux voies.
fn pcm_source(amplitude: f64) -> Vec<u8> {
    let n = (SR * SECONDES) as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f64 / SR as f64;
        let v = ((2.0 * std::f64::consts::PI * 1000.0 * t).sin() * amplitude * 32767.0) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

/// Une bande à +6 dB sur 1 kHz : la réserve anti-écrêtage retire 6 dB, le
/// niveau moyen (bruit rose) en perd plusieurs.
fn profil() -> crate::audio::eq::EqProfile {
    crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 1000.0,
            gain: 6.0,
            q: 1.0,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn rms_db(pcm: &[u8]) -> f64 {
    let (mut e, mut n) = (0.0f64, 0u64);
    for s in pcm.chunks_exact(2) {
        let v = i16::from_le_bytes([s[0], s[1]]) as f64 / 32_768.0;
        e += v * v;
        n += 1;
    }
    10.0 * (e / n.max(1) as f64).max(1e-30).log10()
}

fn au_rail(pcm: &[u8]) -> usize {
    pcm.chunks_exact(2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]))
        .filter(|&v| v == i16::MAX || v == i16::MIN)
        .count()
}

struct Montage {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    source: String,
    amplitude: f64,
    _dir: tempfile::TempDir,
}

async fn monter(amplitude: f64, device: &str, egaliseur: bool) -> Montage {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("piste.flac");
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&pcm_source(amplitude)).await.unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let source = piste.to_string_lossy().into_owned();
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'T', 1, 1, ?, 'flac', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&source as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let type_sortie = if device.starts_with("local:") {
        "local"
    } else {
        "dlna"
    };
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zones
        .create("ND8006", Some(type_sortie), Some(device))
        .unwrap();
    // FLAC natif : la sortie factice n'a pas de Sink, la négociation
    // basculerait sinon en WAV.
    zones.update_dlna_native_flac(zone_id, true).unwrap();
    if egaliseur {
        let s = SettingsRepo::with_backend(orch.db.clone());
        s.set("plugin_equalizer_installed", "true").unwrap();
        s.set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profil()).unwrap(),
        )
        .unwrap();
        assert!(orch.zone_has_active_eq(zone_id));
    }
    Montage {
        orch,
        zone_id,
        source,
        amplitude,
        _dir: dir,
    }
}

fn couper_la_compensation(m: &Montage) {
    SettingsRepo::with_backend(m.orch.db.clone())
        .set(
            &PlaybackOrchestrator::cle_compensation_de_niveau(m.zone_id),
            "false",
        )
        .unwrap();
}

/// Ce que le renderer reçoit : le PCM décodé du fichier servi.
struct Servi {
    pcm: Vec<u8>,
}

async fn jouer(m: &Montage) -> Servi {
    let req = PlayRequest {
        zone_id: m.zone_id,
        output_device_id: Some(
            ZoneRepo::with_backend(m.orch.db.clone())
                .get(m.zone_id)
                .unwrap()
                .unwrap()
                .output_device_id
                .unwrap(),
        ),
        track_id: Some(1),
        source: Some("local".into()),
        ..Default::default()
    };
    let r = m.orch.resolve_local_track(&req).await.unwrap();
    let sid = r.stream_id.clone().expect("une session");
    let session = m
        .orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session inscrite");
    let chemin = session
        .file_path
        .lock()
        .await
        .clone()
        .expect("une session FICHIER : l'égaliseur ré-encode la piste entière");
    let d = crate::audio::decode::decode_to_pcm(&chemin, None, None, 0.0, 0.0).unwrap();
    assert_eq!((d.channels, d.bit_depth), (2, 16));
    Servi { pcm: d.pcm_bytes() }
}

/// L'étalon : la source passée par l'égaliseur SEUL — les octets que le code
/// d'avant #5071 servait.
fn egaliseur_seul(m: &Montage) -> Vec<u8> {
    let mut pcm = pcm_source(m.amplitude);
    crate::audio::eq::EqProcessor::new(&profil(), SR, 2).process_pcm(&mut pcm, 16);
    pcm
}

/// Signal BAS (−26 dBFS) : ses crêtes ont toute la place, la compensation
/// entière est rendue — le niveau servi dépasse celui de l'égaliseur seul
/// d'EXACTEMENT la cible publiée.
#[tokio::test]
async fn le_flux_reseau_porte_toute_la_compensation_quand_la_crete_le_permet_5071() {
    let m = monter(0.05, DEVICE, true).await;
    let cible = PlaybackOrchestrator::compensation_reseau_prevue_with(&m.orch.db, m.zone_id)
        .expect("une zone DLNA avec égaliseur doit être compensée");
    let servi = jouer(&m).await;
    let etalon = egaliseur_seul(&m);
    let rendu = rms_db(&servi.pcm) - rms_db(&etalon);
    println!("#5071 signal bas : cible {cible:.2} dB, rendu {rendu:.2} dB");
    assert!(
        cible > 3.0,
        "l'égaliseur doit retirer du niveau : cible {cible}"
    );
    assert!(
        (rendu - cible).abs() < 0.1,
        "le flux doit rendre la compensation ({cible:.2} dB), il rend {rendu:.2} dB"
    );
    assert_eq!(au_rail(&servi.pcm), 0);
}

/// Signal FORT (−0,9 dBFS) : rendre toute la cible écrêterait de plusieurs dB.
/// La réserve borne le gain à la crête : AUCUN échantillon au rail, la crête
/// sous −0,1 dBFS — et le gain rendu reste positif.
#[tokio::test]
async fn un_signal_fort_est_compense_sans_un_echantillon_ecrete_5071() {
    let m = monter(0.9, DEVICE, true).await;
    let cible =
        PlaybackOrchestrator::compensation_reseau_prevue_with(&m.orch.db, m.zone_id).unwrap();
    let servi = jouer(&m).await;
    let etalon = egaliseur_seul(&m);
    let rendu = rms_db(&servi.pcm) - rms_db(&etalon);
    let crete = crete_relative(&servi.pcm, 16);
    let sans_reserve = 20.0 * crete_relative(&etalon, 16).log10() + cible;
    println!(
        "#5071 signal fort : cible {cible:.2} dB, rendu {rendu:.2} dB, crête {:.2} dBFS \
         (sans réserve : {sans_reserve:.2} dBFS)",
        20.0 * crete.log10()
    );
    assert!(sans_reserve > 0.5, "le témoin doit exiger la réserve");
    assert_eq!(au_rail(&servi.pcm), 0, "échantillons écrêtés");
    assert!(
        crete <= 10f64.powf(PLAFOND_DBFS / 20.0) + 2.0 / 32_768.0,
        "crête {crete}"
    );
    assert!(
        rendu > 0.3,
        "gain rendu {rendu:.2} dB : la compensation n'est pas appliquée"
    );
    assert!(
        rendu < cible,
        "la réserve doit retenir une partie de la cible"
    );
}

/// Interrupteur COUPÉ : le flux est celui d'avant, octet pour octet (le PCM
/// servi est exactement l'égaliseur seul).
#[tokio::test]
async fn compensation_coupee_le_flux_est_inchange_5071() {
    let m = monter(0.05, DEVICE, true).await;
    couper_la_compensation(&m);
    assert!(PlaybackOrchestrator::compensation_reseau_prevue_with(&m.orch.db, m.zone_id).is_none());
    let servi = jouer(&m).await;
    assert!(servi.pcm == egaliseur_seul(&m), "le flux a changé");
}

/// Sans égaliseur ni crossfeed : la compensation (active par défaut) n'a rien
/// à rendre. La piste part TELLE QUELLE — bit-perfect conservé.
#[tokio::test]
async fn sans_dsp_la_piste_part_telle_quelle_5071() {
    let m = monter(0.9, DEVICE, false).await;
    assert!(PlaybackOrchestrator::compensation_reseau_prevue_with(&m.orch.db, m.zone_id).is_none());
    let req = PlayRequest {
        zone_id: m.zone_id,
        output_device_id: Some(DEVICE.into()),
        track_id: Some(1),
        source: Some("local".into()),
        ..Default::default()
    };
    let r = m.orch.resolve_local_track(&req).await.unwrap();
    let sid = r.stream_id.clone().expect("une session");
    let session = m
        .orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .unwrap();
    let chemin = session
        .file_path
        .lock()
        .await
        .clone()
        .expect("fichier servi");
    assert_eq!(
        std::fs::read(&chemin).unwrap(),
        std::fs::read(&m.source).unwrap(),
        "sans traitement, le fichier servi doit être la source"
    );
}

/// Le relais PROGRESSIF (radio, services, WAV à la volée) porte le même étage,
/// en dernier, et ne l'arme jamais pour une sortie locale ni en PURE.
#[tokio::test]
async fn le_relais_progressif_porte_la_compensation_sans_ecreter_5071() {
    let m = monter(0.9, DEVICE, true).await;
    let mut chaine = m.orch.load_streaming_dsp(m.zone_id, None, SR, 2);
    assert!(chaine.compensation.is_some(), "zone DLNA + égaliseur");
    let mut etalon = pcm_source(0.9);
    let mut eq_seul = crate::audio::eq::EqProcessor::new(&profil(), SR, 2);
    let mut servi = Vec::new();
    for bloc in etalon.chunks_mut(8192) {
        let mut b = bloc.to_vec();
        chaine.process(&mut b, 16);
        servi.extend_from_slice(&b);
        eq_seul.process_pcm(bloc, 16);
    }
    let comp = chaine.compensation.as_ref().unwrap();
    println!(
        "#5071 relais : cible {:.2} dB, appliqué {:.2} dB, rabots {}",
        comp.cible_db(),
        comp.gain_applique_db(),
        comp.rabots()
    );
    assert_eq!(comp.ecretage().echantillons_ecretes, 0);
    assert_eq!(au_rail(&servi), 0);
    assert!(rms_db(&servi) - rms_db(&etalon) > 0.3, "gain non appliqué");

    // Sortie locale : elle compense par son volume, jamais dans le flux.
    let local = monter(0.9, "local:Casque", true).await;
    assert!(
        local
            .orch
            .load_streaming_dsp(local.zone_id, None, SR, 2)
            .compensation
            .is_none()
    );

    // PURE : ni égaliseur, ni compensation.
    SettingsRepo::with_backend(m.orch.db.clone())
        .set(
            &format!("zone_{}_audiophile", m.zone_id),
            r#"{"enabled":true}"#,
        )
        .unwrap();
    let pure = m.orch.load_streaming_dsp(m.zone_id, None, SR, 2);
    assert!(pure.compensation.is_none() && !pure.is_active());
}

/// Basculer l'interrupteur en cours de lecture sur une zone réseau doit
/// REFABRIQUER le flux (ses octets changent) — par la garde de #4407, comme un
/// changement de profil. Sans égaliseur, il ne change rien : le flux est
/// CONSERVÉ, sans coupure.
#[tokio::test]
async fn basculer_la_compensation_refabrique_le_flux_seulement_s_il_change_5071() {
    let np = crate::playback::NowPlaying {
        stream_id: Some("sid-5071".into()),
        ..Default::default()
    };

    let m = monter(0.5, DEVICE, true).await;
    let avant = m.orch.empreinte_du_traitement(m.zone_id, None);
    m.orch
        .noter_traitement_du_flux(m.zone_id, "sid-5071", avant);
    assert!(m.orch.flux_porte_deja_ce_traitement(m.zone_id, &np));
    couper_la_compensation(&m);
    assert!(
        !m.orch.flux_porte_deja_ce_traitement(m.zone_id, &np),
        "égaliseur actif : couper la compensation change les octets, le flux doit être refabriqué"
    );

    let sans = monter(0.5, DEVICE, false).await;
    let avant = sans.orch.empreinte_du_traitement(sans.zone_id, None);
    sans.orch
        .noter_traitement_du_flux(sans.zone_id, "sid-5071", avant);
    couper_la_compensation(&sans);
    assert!(
        sans.orch.flux_porte_deja_ce_traitement(sans.zone_id, &np),
        "sans égaliseur ni crossfeed, l'interrupteur ne change rien : flux conservé"
    );
}
