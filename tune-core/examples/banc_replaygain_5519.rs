//! Banc de débit de la passe ReplayGain de fond (#5519).
//!
//! Tades mesure ~1 300 pistes/h sur 400 000. Ce binaire dit OÙ part le temps,
//! sur de vrais fichiers :
//!
//! - **A — étages par fichier** : décodage seul (mêmes segments de 30 s que
//!   l'analyse), mesure complète (`mesurer_intensite_et_plage` : décodage +
//!   sonie + crête vraie + DR), empreinte (`empreinte_du_fichier`) ;
//! - **B — passe réelle** : `analyze_track_batch` sur une base SQLite sur
//!   disque (migrations complètes), jusqu'à épuisement — écritures, pauses et
//!   empreinte comprises ;
//! - **C — requêtes sur une grande bibliothèque** : base synthétique de
//!   `--pistes` lignes, coût des sélections qui précèdent chaque lot ;
//! - **D — parallélisme** : la mesure complète sur K fichiers à la fois.
//!
//! ```text
//! cargo run --release -p tune-core --example banc_replaygain_5519 -- <dossier> [max_fichiers] [pistes_synthetiques]
//! ```

use std::sync::Arc;
use std::time::Instant;
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;

fn lister(dossier: &std::path::Path, max: usize) -> Vec<String> {
    let mut v = Vec::new();
    let mut pile = vec![dossier.to_path_buf()];
    while let Some(d) = pile.pop() {
        let Ok(it) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut ent: Vec<_> = it.flatten().map(|e| e.path()).collect();
        ent.sort();
        for p in ent {
            if p.is_dir() {
                pile.push(p);
            } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(e.to_lowercase().as_str(), "flac" | "mp3" | "m4a" | "wav")
            }) {
                v.push(p.to_string_lossy().to_string());
            }
        }
    }
    v.sort();
    v.truncate(max);
    v
}

fn decoder_seul(chemin: &str) -> f64 {
    let mut seek = 0.0f64;
    let mut duree = 0.0;
    while let Ok(d) = tune_core::audio::decode::decode_to_pcm(chemin, None, Some(2), seek, 30.0) {
        let (sr, ch) = (d.sample_rate as usize, d.channels as usize);
        if sr == 0 || ch == 0 || d.samples_i32.is_empty() {
            break;
        }
        let frames = d.samples_i32.len() / ch;
        duree += frames as f64 / sr as f64;
        if (frames as f64) < 30.0 * sr as f64 {
            break;
        }
        seek += frames as f64 / sr as f64;
    }
    duree
}

fn base_sur_disque(dossier: &std::path::Path, nom: &str) -> (SqliteDb, Arc<dyn DbBackend>) {
    let f = dossier.join(nom);
    let _ = std::fs::remove_file(&f);
    let db = SqliteDb::open(f.to_str().unwrap()).expect("base");
    db.init_schema().expect("schema");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    let b: Arc<dyn DbBackend> = Arc::new(db.clone());
    b.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('replaygain_mode', 'track')",
        &[],
    )
    .unwrap();
    (db, b)
}

fn chrono<T>(nom: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    println!("  {nom:<58} {:>9.1} ms", t.elapsed().as_secs_f64() * 1e3);
    r
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dossier = std::path::PathBuf::from(args.get(1).expect("dossier"));
    let max: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let pistes_synth: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500_000);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let fichiers = lister(&dossier, max);
    println!("fichiers: {}", fichiers.len());
    let tmp = tempfile::TempDir::new().unwrap();

    if max > 0 {
        // ── A ─────────────────────────────────────────────────────────────────
        let (mut t_dec, mut t_mes, mut t_emp, mut audio) = (0.0, 0.0, 0.0, 0.0);
        for f in &fichiers {
            let t = Instant::now();
            let d = decoder_seul(f);
            t_dec += t.elapsed().as_secs_f64();
            audio += d;
            let t = Instant::now();
            let m = rt.block_on(tune_core::audio::analyzer::mesurer_intensite_et_plage(f));
            t_mes += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let _ = tune_core::audio::empreinte::empreinte_du_fichier(f);
            t_emp += t.elapsed().as_secs_f64();
            // Résultat exact (Debug f64 = aller-retour sans perte) : c'est lui qui
            // prouve qu'une optimisation rend les MÊMES valeurs au bit près.
            eprintln!("R {f} {m:?}");
        }
        let n = fichiers.len().max(1) as f64;
        println!(
            "A — {n} fichiers, {:.0} s d'audio ({:.0} s/piste)",
            audio,
            audio / n
        );
        println!(
            "  decodage seul      {:>7.3} s/piste  {:>6.0} xRT",
            t_dec / n,
            audio / t_dec
        );
        println!(
            "  mesure complete    {:>7.3} s/piste  {:>6.0} xRT",
            t_mes / n,
            audio / t_mes
        );
        println!("  empreinte          {:>7.3} s/piste", t_emp / n);
        println!(
            "  calcul (mesure - decodage) {:>7.3} s/piste",
            (t_mes - t_dec) / n
        );

        if std::env::var("BANC_SEUL_A").is_ok() {
            return;
        }
        // ── B ─────────────────────────────────────────────────────────────────
        let (_db, b) = base_sur_disque(tmp.path(), "passe.db");
        for (i, f) in fichiers.iter().enumerate() {
            let id = (i + 1) as i64;
            b.execute(
            "INSERT INTO tracks (id, title, file_path, duration_ms, sample_rate, channels, format) \
             VALUES (?, 't', ?, 240000, 44100, 2, 'flac')",
            &[&id as &dyn ToSqlValue, f as &dyn ToSqlValue],
        )
        .unwrap();
        }
        let t = Instant::now();
        let mut total = 0;
        loop {
            let k = rt.block_on(tune_core::audio::replaygain::analyze_track_batch(&b));
            if k == 0 {
                break;
            }
            total += k;
        }
        let mur = t.elapsed().as_secs_f64();
        println!(
            "B — analyze_track_batch : {total} pistes en {mur:.1} s = {:.3} s/piste = {:.0} pistes/h",
            mur / total.max(1) as f64,
            total as f64 * 3600.0 / mur
        );
        println!(
            "  dont pauses fixes 400 ms : {:.3} s/piste ; reste hors mesure+empreinte : {:.3} s/piste",
            0.4,
            mur / total.max(1) as f64 - 0.4 - t_mes / n - t_emp / n
        );

        // ── D ─────────────────────────────────────────────────────────────────
        for k in [1usize, 2, 4] {
            let t = Instant::now();
            rt.block_on(async {
                for bloc in fichiers.chunks(k) {
                    let h: Vec<_> = bloc
                        .iter()
                        .map(|f| {
                            let f = f.clone();
                            tokio::spawn(async move {
                                tune_core::audio::analyzer::mesurer_intensite_et_plage(&f).await
                            })
                        })
                        .collect();
                    for x in h {
                        let _ = x.await;
                    }
                }
            });
            let s = t.elapsed().as_secs_f64();
            println!(
                "D — mesure complete, {k} a la fois : {:.3} s/piste ({:.2}x)",
                s / n,
                t_mes / s
            );
        }
    }
    // ── C ─────────────────────────────────────────────────────────────────
    let (db, b) = base_sur_disque(tmp.path(), "grande.db");
    println!("C — base synthetique de {pistes_synth} pistes");
    chrono("remplissage (hors mesure)", || {
        db.execute_batch("BEGIN").unwrap();
        for album in 1..=(pistes_synth / 12 + 1) {
            b.execute(
                "INSERT INTO albums (id, title) VALUES (?, 'a')",
                &[&album as &dyn ToSqlValue],
            )
            .unwrap();
        }
        for id in 1..=pistes_synth {
            let chemin = format!("/nulle/part/{id}.flac");
            let album = id / 12 + 1;
            b.execute(
                "INSERT INTO tracks (id, title, album_id, file_path, duration_ms, sample_rate, channels, format) \
                 VALUES (?, 't', ?, ?, 240000, 44100, 2, 'flac')",
                &[&id as &dyn ToSqlValue, &album as &dyn ToSqlValue, &chemin as &dyn ToSqlValue],
            )
            .unwrap();
            // Tout est déjà vu par la passe : rg_analyzed + gain, et la moitié
            // porte un DR. Les sélections doivent alors parcourir la table.
            for (k, v) in [("rg_analyzed", "1"), ("rg_track_gain", "-3.00 dB")] {
                b.execute(
                    "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
                    &[
                        &id as &dyn ToSqlValue,
                        &k as &dyn ToSqlValue,
                        &v as &dyn ToSqlValue,
                    ],
                )
                .unwrap();
            }
            if id % 2 == 0 {
                b.execute(
                    "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'dr_track', '12')",
                    &[&id as &dyn ToSqlValue],
                )
                .unwrap();
            } else {
                b.execute(
                    "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'dr_indisponible', '1')",
                    &[&id as &dyn ToSqlValue],
                )
                .unwrap();
            }
        }
        db.execute_batch("COMMIT").unwrap();
    });
    chrono("analyze_track_batch (0 candidat : requete seule)", || {
        rt.block_on(tune_core::audio::replaygain::analyze_track_batch(&b))
    });
    chrono(
        "rattraper_un_lot_de_dr (0 candidat : requete seule)",
        || rt.block_on(tune_core::audio::replaygain::rattraper_un_lot_de_dr(&b)),
    );
    chrono(
        "empreinter_un_lot (tous candidats, chemins absents)",
        || rt.block_on(tune_core::audio::replaygain::empreinter_un_lot(&b)),
    );
    chrono("analyze_album_batch (tous albums a faire)", || {
        tune_core::audio::replaygain::analyze_album_batch(&b)
    });
    chrono("compter_les_candidats_replaygain", || {
        tune_core::audio::replaygain::compter_les_candidats_replaygain(&b)
    });
    chrono("compter_les_candidats_dr", || {
        tune_core::audio::replaygain::compter_les_candidats_dr(&b)
    });
    // Un seul fichier synthétique à analyser : la requête de candidats doit
    // parcourir la table pour le trouver (lot de 1 au lieu de 25).
    b.execute(
        "DELETE FROM track_metadata WHERE track_id = ?",
        &[&pistes_synth as &dyn ToSqlValue],
    )
    .unwrap();
    chrono("analyze_track_batch (1 candidat en fin de table)", || {
        rt.block_on(tune_core::audio::replaygain::analyze_track_batch(&b))
    });
    // Écritures : 25 pistes × les clés de la mesure, une transaction par clé.
    let repo = tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(b.clone());
    chrono("200 ecritures track_metadata (25 pistes x 8)", || {
        for id in 1..=25i64 {
            for k in [
                "rg_track_gain",
                "rg_track_peak",
                "rg_track_true_peak",
                "rg_source",
                "dr_track",
                "dr_source",
                "rg_analyzed",
                "x",
            ] {
                let _ = repo.set(id, k, "1");
            }
        }
    });
}
