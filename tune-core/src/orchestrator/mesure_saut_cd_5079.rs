//! MESURE À LA MAIN (#5079), jamais en CI : saut de piste sur un VRAI CD
//! monté par macOS (`/Volumes/Audio CD`), par le vrai chemin
//! `resolve_stream` → `resolve_source_pcm` → pompe → session. Aucune zone,
//! aucun son : on observe le remplissage du canal de la session.
//! `MESURE_CD_DECALAGE_S` décale toutes les positions (pas de cache),
//! `MESURE_CD_JOUE` / `MESURE_CD_VISEE` choisissent les pistes.
//!
//! ```text
//! cargo test -p tune-core --lib mesure_saut_cd_5079 -- --ignored --nocapture
//! ```
//!
//! Mesuré le 26/09/2026 (Apple SuperDrive, *A Trick of the Tail*, piste 1
//! → piste 7, cinq essais), 5 s d'audio dans la session nouvelle :
//! 16,4 à 17,2 s avant le correctif (0,30×), 0,88 à 0,99 s après (5,1 à
//! 5,7×). L'ancienne session n'est pas retirée pendant la mesure : c'est le
//! pire cas, le serveur la retire quand l'ancien consommateur la lâche.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};
use crate::streaming::registry::ServiceRegistry;

use super::{PlayRequest, PlaybackOrchestrator};

const VOLUME: &str = "/Volumes/Audio CD";

/// Lit la piste N directement dans le fichier AIFF du volume `cddafs` :
/// les mêmes lectures de fichier que `LecteurVolume` (lot cd-macos).
struct AiffDuVolume {
    lus: Vec<Arc<AtomicU64>>,
}

fn debut_ssnd(f: &mut std::fs::File) -> (u64, u64) {
    let mut entete = [0u8; 12];
    f.read_exact(&mut entete).unwrap();
    let mut pos = 12u64;
    loop {
        let mut c = [0u8; 8];
        f.seek(SeekFrom::Start(pos)).unwrap();
        f.read_exact(&mut c).unwrap();
        let taille = u32::from_be_bytes(c[4..8].try_into().unwrap()) as u64;
        if &c[..4] == b"SSND" {
            let mut o = [0u8; 4];
            f.read_exact(&mut o).unwrap();
            let decalage = u32::from_be_bytes(o) as u64;
            return (pos + 16 + decalage, taille - 8 - decalage);
        }
        pos += 8 + taille + (taille & 1);
    }
}

struct Compte {
    /// Blocs de 24 secteurs, comme `FluxPiste` (56 448 octets).
    f: std::io::BufReader<std::fs::File>,
    lus: Arc<AtomicU64>,
}

impl Read for Compte {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.f.read(buf)?;
        self.lus.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl FournisseurPcm for std::sync::Mutex<AiffDuVolume> {
    fn ouvrir(&self, source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String> {
        let n: u8 = source_id.parse().unwrap();
        // Comme `LecteurVolume::lire_toc` : relire la TOC à chaque ouverture.
        let _ = std::fs::read(format!("{VOLUME}/.TOC.plist")).map_err(|e| e.to_string())?;
        let mut f = std::fs::File::open(format!("{VOLUME}/{n} Audio Track.aiff"))
            .map_err(|e| e.to_string())?;
        let (debut, octets) = debut_ssnd(&mut f);
        let saut = (depuis_ms * 44_100 / 1000 * 4).min(octets);
        f.seek(SeekFrom::Start(debut + saut)).unwrap();
        let lus = Arc::new(AtomicU64::new(0));
        self.lock().unwrap().lus.push(lus.clone());
        Ok(FluxPcm {
            format: FormatPcm::CD,
            octets: octets - saut,
            duree_ms: octets * 1000 / FormatPcm::CD.octets_par_seconde(),
            lecteur: Box::new(Compte {
                f: std::io::BufReader::with_capacity(24 * 2352, f),
                lus,
            }),
        })
    }
}

async fn troncons(orch: &PlaybackOrchestrator, sid: &str) -> usize {
    let s = orch.streamer.sessions_state();
    let g = s.lock().await;
    match g.get(sid) {
        Some(s) => s.channel_fill().await.map(|(n, _)| n).unwrap_or(0),
        None => 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "mesure à la main sur un vrai CD monté"]
async fn mesure_saut_cd_5079() {
    let decalage: u64 = std::env::var("MESURE_CD_DECALAGE_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let orch = PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let disque = Arc::new(std::sync::Mutex::new(AiffDuVolume { lus: Vec::new() }));
    orch.sources_pcm().inscrire("cd-mesure", disque.clone());
    let zone_id = 5079;
    let piste = |v: &str, d: u8| -> u8 {
        std::env::var(v)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    let (p_joue, p_visee) = (piste("MESURE_CD_JOUE", 4), piste("MESURE_CD_VISEE", 8));
    // 32 Kio par tronçon : 200 ms = 2 tronçons, 5 s = 27 tronçons.
    println!("rep\tresolution_ms\t200ms_ms\t5s_ms\tdebit_x\tancienne_lit_apres_Kio");
    for rep in 0..5u64 {
        orch.playback.play(zone_id, NowPlaying::default()).await;
        let req = |piste: u8, depuis_s: u64| PlayRequest {
            zone_id,
            source: Some("cd-mesure".into()),
            source_id: Some(piste.to_string()),
            seek_ms: Some(depuis_s * 1000),
            ..Default::default()
        };
        // La piste 4 joue depuis 3 s…
        let a = orch
            .resolve_stream(&req(p_joue, 20 + decalage + rep * 25))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let i_ancienne = disque.lock().unwrap().lus.len() - 1;
        // …l'auditeur choisit la piste 8.
        let t0 = Instant::now();
        let b = orch
            .resolve_stream(&req(p_visee, decalage + rep * 30))
            .await
            .unwrap();
        let resolution = t0.elapsed();
        let ancienne_a_la_resolution =
            disque.lock().unwrap().lus[i_ancienne].load(Ordering::SeqCst);
        let sid = b.stream_id.unwrap();
        let (mut t200, mut t5) = (None, None);
        let mut n = 0;
        let mut courbe = Vec::new();
        while t5.is_none() && t0.elapsed() < Duration::from_secs(30) {
            n = troncons(&orch, &sid).await;
            if courbe.last().map(|(_, m)| *m != n).unwrap_or(true) {
                courbe.push((t0.elapsed().as_millis(), n));
            }
            if t200.is_none() && n >= 2 {
                t200 = Some(t0.elapsed());
            }
            if n >= 27 {
                t5 = Some(t0.elapsed());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let ancienne_apres = disque.lock().unwrap().lus[i_ancienne].load(Ordering::SeqCst)
            - ancienne_a_la_resolution;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let t5 = t5.unwrap_or(t0.elapsed());
        println!(
            "{rep}\t{:.0}\t{:.0}\t{:.0}\t{:.2}\t{}\t{n} tronçons\t{:?}",
            ms(resolution),
            t200.map(ms).unwrap_or(-1.0),
            ms(t5),
            n as f64 * 32768.0 / 176_400.0 / t5.as_secs_f64(),
            ancienne_apres / 1024,
            courbe.iter().step_by(4).collect::<Vec<_>>()
        );
        orch.streamer.remove_session(&sid).await;
        if let Some(s) = a.stream_id {
            orch.streamer.remove_session(&s).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
