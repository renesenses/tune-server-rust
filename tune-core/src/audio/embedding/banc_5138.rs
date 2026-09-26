//! Banc de mesure de #5138 : ce que coûte la passe acoustique au reste du
//! serveur pendant qu'elle tourne. IGNORÉ par défaut : il télécharge le vrai
//! modèle CLAP (287 Mo) et le runtime onnxruntime, puis analyse de vraies
//! pistes. Il MESURE et imprime ; il n'affirme rien.
//!
//! ```text
//! TUNE_BANC_CLAP_DIR=/chemin/persistant BANC_PISTES=300 BANC_OUVRIERS=4 \
//! BANC_DEBIT=equilibre BANC_SECONDES=90 \
//!   cargo test --release -p tune-core --features audio-embedding --lib \
//!     banc_5138 -- --ignored --nocapture
//! ```
//!
//! Quatre relevés, pendant que la passe travaille :
//! - le RETARD de l'exécuteur : une sonde dort 5 ms en boucle et note de
//!   combien elle se réveille en retard ;
//! - un FLUX de lecture simulé : une tâche de l'exécuteur remplit un anneau
//!   de 200 ms, un fil « pilote » le vide en temps réel toutes les 10 ms ; un
//!   pilote qui trouve l'anneau vide compte une FAMINE — le
//!   `famine_anneau_debut` du testeur ;
//! - les CŒURS occupés (temps processeur du processus / temps réel) et le
//!   nombre de fils en état R ;
//! - la MÉMOIRE résidente.
//!
//! Puis une zone se met à jouer : combien de temps la passe met-elle à rendre
//! la main ?

use super::*;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn env_ou<T: std::str::FromStr>(cle: &str, defaut: T) -> T {
    std::env::var(cle)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(defaut)
}

/// Un WAV 44,1 kHz / 16 bits / stéréo, au contenu différent d'une piste à
/// l'autre (le modèle n'y voit que du bruit coloré, ce qui suffit à le faire
/// travailler exactement autant qu'une vraie musique : la fenêtre est fixe).
fn ecrire_wav(path: &std::path::Path, secondes: usize, graine: u32) {
    const HZ: usize = 44_100;
    let frames = HZ * secondes;
    let mut donnees = Vec::with_capacity(frames * 4);
    let mut x = graine.wrapping_mul(2_654_435_761).max(1);
    for f in 0..frames {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let bruit = (x >> 20) as i32 - 2048;
        let ton = ((f as f32 * (220.0 + graine as f32) * std::f32::consts::TAU / HZ as f32).sin()
            * 8000.0) as i32;
        let g = (ton + bruit).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        donnees.extend_from_slice(&g.to_le_bytes());
        donnees.extend_from_slice(&(g / 2).to_le_bytes());
    }
    let mut w = Vec::with_capacity(donnees.len() + 44);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&(HZ as u32).to_le_bytes());
    w.extend_from_slice(&((HZ * 4) as u32).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
    w.extend_from_slice(&donnees);
    std::fs::write(path, w).unwrap();
}

/// Temps processeur du processus, en tops d'horloge (utime + stime).
fn tops_processeur() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let apres = stat.rsplit_once(')').map(|(_, a)| a).unwrap_or("");
    let champs: Vec<&str> = apres.split_whitespace().collect();
    // Après la parenthèse : état (0), … utime (11), stime (12).
    let u: u64 = champs.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let s: u64 = champs.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    u + s
}

/// Fils du processus en état R à l'instant du relevé.
fn fils_en_cours() -> usize {
    let Ok(taches) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    taches
        .filter_map(|t| t.ok())
        .filter(|t| {
            std::fs::read_to_string(t.path().join("stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')
                        .map(|(_, a)| a.trim_start().starts_with('R'))
                })
                .unwrap_or(false)
        })
        .count()
}

fn quantile(v: &mut [u64], q: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * q).round() as usize;
    v[i]
}

#[test]
#[ignore]
fn banc_5138() {
    let ouvriers: usize = env_ou("BANC_OUVRIERS", 4);
    let pistes: usize = env_ou("BANC_PISTES", 300);
    let secondes: u64 = env_ou("BANC_SECONDES", 90);
    let debit: String = env_ou("BANC_DEBIT", "equilibre".to_string());
    let racine: std::path::PathBuf = std::env::var("TUNE_BANC_CLAP_DIR")
        .map(Into::into)
        .unwrap_or_else(|_| std::env::temp_dir().join("banc-clap-5138"));
    std::fs::create_dir_all(racine.join("pistes")).unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(ouvriers)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let modele = racine.join("clap-audio-music-2023.onnx");
        ensure_model(&modele).await.expect("modèle");
        ensure_runtime_loaded(&modele).await.expect("runtime");

        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO zones (id, name, last_play_state) VALUES (1, 'Banc', 'stopped')",
            &[],
        )
        .unwrap();
        for i in 1..=pistes {
            let p = racine.join("pistes").join(format!("{i:04}.wav"));
            if !p.exists() {
                ecrire_wav(&p, 12, i as u32);
            }
            let chemin = p.to_string_lossy().to_string();
            let id = i as i64;
            db.execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format) \
                 VALUES (?, 'T', 1, 1, ?, 'wav')",
                &[&id, &chemin],
            )
            .unwrap();
        }
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        let reglages = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        reglages.set(THROTTLE_KEY, &debit).unwrap();
        let fils_onnx = intra_threads_for(&reglages);

        let rss_avant = process_rss_mb().unwrap_or(0);
        let t_charge = Instant::now();
        let mut embedder = charger_pour_le_banc(&modele, fils_onnx);
        let charge_ms = t_charge.elapsed().as_millis();
        let rss_charge = process_rss_mb().unwrap_or(0);

        let arret = Arc::new(AtomicBool::new(false));

        // Sonde de l'exécuteur.
        let retards = Arc::new(StdMutex::new(Vec::<u64>::new()));
        {
            let (arret, retards) = (arret.clone(), retards.clone());
            tokio::spawn(async move {
                while !arret.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let retard = t.elapsed().saturating_sub(Duration::from_millis(5));
                    retards.lock().unwrap().push(retard.as_micros() as u64);
                }
            });
        }

        // Flux simulé : producteur sur l'exécuteur, pilote sur son propre fil.
        let niveau = Arc::new(AtomicI64::new(20));
        let famines = Arc::new(AtomicU64::new(0));
        let manque_ms = Arc::new(AtomicU64::new(0));
        {
            let (arret, niveau) = (arret.clone(), niveau.clone());
            tokio::spawn(async move {
                let mut cadence = tokio::time::interval(Duration::from_millis(10));
                cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
                while !arret.load(Ordering::Relaxed) {
                    cadence.tick().await;
                    if niveau.load(Ordering::Relaxed) < 20 {
                        niveau.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        let pilote = {
            let (arret, niveau, famines, manque_ms) = (
                arret.clone(),
                niveau.clone(),
                famines.clone(),
                manque_ms.clone(),
            );
            std::thread::spawn(move || {
                let mut en_famine = false;
                let debut = Instant::now();
                let mut n = 0u64;
                while !arret.load(Ordering::Relaxed) {
                    n += 1;
                    let cible = debut + Duration::from_millis(10 * n);
                    if let Some(d) = cible.checked_duration_since(Instant::now()) {
                        std::thread::sleep(d);
                    }
                    if niveau.load(Ordering::Relaxed) > 0 {
                        niveau.fetch_sub(1, Ordering::Relaxed);
                        en_famine = false;
                    } else {
                        manque_ms.fetch_add(10, Ordering::Relaxed);
                        if !en_famine {
                            famines.fetch_add(1, Ordering::Relaxed);
                            en_famine = true;
                        }
                    }
                }
            })
        };

        // Cœurs, fils R, mémoire.
        let releves = Arc::new(StdMutex::new(Vec::<(f64, usize, u64)>::new()));
        let echantillonneur = {
            let (arret, releves) = (arret.clone(), releves.clone());
            std::thread::spawn(move || {
                let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
                let mut t0 = Instant::now();
                let mut c0 = tops_processeur();
                while !arret.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    let (t1, c1) = (Instant::now(), tops_processeur());
                    let coeurs = (c1 - c0) as f64 / hz / (t1 - t0).as_secs_f64();
                    releves.lock().unwrap().push((
                        coeurs,
                        fils_en_cours(),
                        process_rss_mb().unwrap_or(0),
                    ));
                    (t0, c0) = (t1, c1);
                }
            })
        };

        // Phase 1 : la passe travaille, rien ne joue. Sur un OUVRIER de
        // l'exécuteur, comme en production (`tokio::spawn` dans `spawn`), et
        // non sur le fil de `block_on`, qui n'en est pas un.
        let debut = Instant::now();
        let (analysees, embedder) = {
            let backend = backend.clone();
            tokio::spawn(async move {
                let mut analysees = 0usize;
                while debut.elapsed() < Duration::from_secs(secondes) {
                    let n = lot_pour_le_banc(&backend, &mut embedder).await;
                    if n == 0 {
                        break;
                    }
                    analysees += n;
                }
                (analysees, embedder)
            })
            .await
            .unwrap()
        };
        let duree_phase1 = debut.elapsed();

        // Phase 2 : une zone se met à jouer en plein lot.
        let lot = {
            let backend = backend.clone();
            let mut e = embedder;
            tokio::spawn(async move {
                let n = lot_pour_le_banc(&backend, &mut e).await;
                (n, Instant::now(), e)
            })
        };
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let t_lecture = Instant::now();
        crate::taches_de_fond::priorite::noter_etat_de_lecture(1, "playing");
        db.execute(
            "UPDATE zones SET last_play_state = 'playing' WHERE id = 1",
            &[],
        )
        .unwrap();
        let (_n, fin_lot, embedder) = lot.await.unwrap();
        let cession_ms = fin_lot.saturating_duration_since(t_lecture).as_millis();
        crate::taches_de_fond::priorite::oublier_la_lecture_pour_les_essais();
        db.execute(
            "UPDATE zones SET last_play_state = 'stopped' WHERE id = 1",
            &[],
        )
        .unwrap();

        // Phase 3 : l'arrêt du serveur tombe en plein lot.
        let lot = {
            let backend = backend.clone();
            let mut e = embedder;
            tokio::spawn(async move {
                lot_pour_le_banc(&backend, &mut e).await;
                Instant::now()
            })
        };
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let t_arret = Instant::now();
        arreter();
        let fin_lot = lot.await.unwrap();
        let arret_ms = fin_lot.saturating_duration_since(t_arret).as_millis();
        reprendre_apres_arret_pour_les_essais();

        arret.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(50)).await;
        pilote.join().unwrap();
        echantillonneur.join().unwrap();

        let mut r = retards.lock().unwrap().clone();
        let n_sondes = r.len();
        let (p50, p99, max) = (
            quantile(&mut r, 0.5),
            quantile(&mut r, 0.99),
            quantile(&mut r, 1.0),
        );
        let sup_100ms = r.iter().filter(|&&x| x > 100_000).count();
        let rel = releves.lock().unwrap().clone();
        let coeurs_moy = rel.iter().map(|x| x.0).sum::<f64>() / rel.len().max(1) as f64;
        let coeurs_max = rel.iter().map(|x| x.0).fold(0.0, f64::max);
        let fils_r_max = rel.iter().map(|x| x.1).max().unwrap_or(0);
        let rss_max = rel.iter().map(|x| x.2).max().unwrap_or(0);

        println!("BANC_5138 ouvriers_tokio={ouvriers} debit={debit} fils_onnx={fils_onnx}");
        println!(
            "BANC_5138 chargement_ms={charge_ms} rss_avant_mb={rss_avant} rss_apres_chargement_mb={rss_charge}"
        );
        println!(
            "BANC_5138 pistes_analysees={analysees} duree_s={:.1} s_par_piste={:.2}",
            duree_phase1.as_secs_f64(),
            duree_phase1.as_secs_f64() / analysees.max(1) as f64
        );
        println!(
            "BANC_5138 retard_executeur_ms p50={:.2} p99={:.1} max={:.1} sondes={n_sondes} sondes_sup_100ms={sup_100ms}",
            p50 as f64 / 1000.0,
            p99 as f64 / 1000.0,
            max as f64 / 1000.0
        );
        println!(
            "BANC_5138 flux famines={} manque_ms={}",
            famines.load(Ordering::Relaxed),
            manque_ms.load(Ordering::Relaxed)
        );
        println!(
            "BANC_5138 coeurs_moyens={coeurs_moy:.2} coeurs_max={coeurs_max:.2} fils_R_max={fils_r_max} rss_max_mb={rss_max}"
        );
        println!("BANC_5138 cession_a_la_lecture_ms={cession_ms}");
        println!("BANC_5138 fin_de_lot_apres_arret_ms={arret_ms}");
    });
}

/// Les deux seuls points du banc qui dépendent de l'API de la passe.
type EmbedderDuBanc = Arc<std::sync::Mutex<AudioEmbedder>>;

fn charger_pour_le_banc(modele: &Path, fils: usize) -> EmbedderDuBanc {
    Arc::new(std::sync::Mutex::new(
        AudioEmbedder::load(modele, fils).expect("chargement du modèle"),
    ))
}

async fn lot_pour_le_banc(backend: &Arc<dyn DbBackend>, e: &mut EmbedderDuBanc) -> usize {
    analyze_embedding_batch(backend, e).await
}
