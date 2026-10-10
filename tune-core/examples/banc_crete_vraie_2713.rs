//! Banc de la crête vraie (#2713) : coût du suréchantillonneur de l'annexe 2
//! face à l'ancien Catmull-Rom, coût du rattrapage par piste, et écart des
//! deux crêtes sur de vrais fichiers, avec son effet sur le gain appliqué.
//!
//! ```text
//! cargo run --release -p tune-core --example banc_crete_vraie_2713 -- <dossier> [max_fichiers]
//! ```
//!
//! Par fichier, le décodage de l'analyse (segments de 30 s, stéréo, cadence
//! native) est fait UNE fois ; les mêmes échantillons nourrissent ensuite
//! chaque estimateur, chronométré à part. Puis la mesure du rattrapage
//! (`mesurer_la_crete_vraie`, décodage compris) et la mesure complète de la
//! passe nominale (`mesurer_intensite_et_plage`) sont chronométrées telles que
//! la passe les appelle.

use std::time::{Duration, Instant};
use tune_core::audio::crete_vraie::{CreteCatmullRom, CreteVraie, crete_vraie_sans_economie};

fn lister(dossier: &std::path::Path, max: usize) -> Vec<String> {
    let mut v = Vec::new();
    let mut pile = vec![dossier.to_path_buf()];
    while let Some(d) = pile.pop() {
        let Ok(it) = std::fs::read_dir(&d) else {
            continue;
        };
        for p in it.flatten().map(|e| e.path()) {
            if p.is_dir() {
                pile.push(p);
            } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(
                    e.to_lowercase().as_str(),
                    "flac" | "mp3" | "m4a" | "wav" | "dsf" | "dff"
                )
            }) {
                v.push(p.to_string_lossy().to_string());
            }
        }
    }
    v.sort();
    v.truncate(max);
    v
}

/// Les segments de l'analyse, convertis en `f64` comme l'analyseur.
fn segments(chemin: &str) -> (usize, Vec<Vec<f64>>, f64) {
    let mut seek = 0.0f64;
    let mut sr = 0;
    let mut v = Vec::new();
    let mut duree = 0.0;
    while let Ok(d) = tune_core::audio::decode::decode_to_pcm(chemin, None, Some(2), seek, 30.0) {
        let ch = d.channels as usize;
        sr = d.sample_rate as usize;
        if sr == 0 || ch == 0 || d.samples_i32.is_empty() {
            break;
        }
        let echelle = match d.bit_depth {
            24 => (1i64 << 23) as f64,
            32 => (1i64 << 31) as f64,
            _ => 32768.0,
        };
        let frames = d.samples_i32.len() / ch;
        v.push(
            d.samples_i32
                .iter()
                .map(|&s| s as f64 / echelle)
                .collect::<Vec<f64>>(),
        );
        duree += frames as f64 / sr as f64;
        if (frames as f64) < 30.0 * sr as f64 {
            break;
        }
        seek += frames as f64 / sr as f64;
    }
    (sr, v, duree)
}

fn chrono<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dossier = std::path::PathBuf::from(args.get(1).expect("dossier"));
    let max: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let fichiers = lister(&dossier, max);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let (mut t_dec, mut t_cr, mut t_fir, mut t_brut, mut t_rattrapage, mut t_complete) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let mut audio_s = 0.0;
    let mut ecarts: Vec<f64> = Vec::new();
    let mut gains_changes: Vec<f64> = Vec::new();
    let mut n = 0usize;
    for f in &fichiers {
        let ((sr, segs, duree), td) = chrono(|| segments(f));
        if segs.is_empty() || sr == 0 {
            continue;
        }
        let (cr, tc) = chrono(|| {
            let mut m = CreteCatmullRom::new(2);
            for s in &segs {
                m.nourrir(s);
            }
            m.crete()
        });
        let (fir, tf) = chrono(|| {
            let mut m = CreteVraie::new(sr, 2);
            for s in &segs {
                m.nourrir(s);
            }
            m.crete()
        });
        // Le pire cas : aucun bloc sauté (un seul appel, même résultat).
        let tout: Vec<f64> = segs.concat();
        let (brut, tb) = chrono(|| crete_vraie_sans_economie(sr, 2, &tout));
        assert_eq!(brut.to_bits(), fir.to_bits(), "{f} : économie inexacte");
        let (seule, tr) =
            chrono(|| rt.block_on(tune_core::audio::analyzer::mesurer_la_crete_vraie(f)));
        let (complete, tm) =
            chrono(|| rt.block_on(tune_core::audio::analyzer::mesurer_intensite_et_plage(f)));
        let Some((lufs, _pic, tp, _dr)) = complete else {
            continue;
        };
        assert_eq!(seule.map(f64::to_bits), Some(tp.to_bits()), "{f}");
        assert_eq!(tp.to_bits(), fir.to_bits(), "{f}");
        n += 1;
        audio_s += duree;
        t_dec += td;
        t_cr += tc;
        t_fir += tf;
        t_brut += tb;
        t_rattrapage += tr;
        t_complete += tm;
        ecarts.push(db(fir) - db(cr));
        // Effet sur le gain appliqué, `prevent_clipping` armé, plafond 0 dBTP,
        // préampli 0 : facteur = min(gain, 1 / crête).
        let gain = 10f64.powf((-18.0 - lufs) / 20.0);
        let avant = gain.min(1.0 / cr);
        let apres = gain.min(1.0 / fir);
        gains_changes.push(db(apres) - db(avant));
        eprintln!(
            "R\t{sr}\t{duree:.1}\tcr={cr:.6}\tfir={fir:.6}\tecart_db={:+.3}\tgain_db={:+.3}",
            db(fir) - db(cr),
            db(apres) - db(avant)
        );
    }
    if n == 0 {
        println!("aucun fichier mesuré");
        return;
    }
    let par_piste = |d: Duration| d.as_secs_f64() / n as f64;
    println!(
        "fichiers mesurés : {n}, audio {:.0} s ({:.1} min/piste)",
        audio_s,
        audio_s / 60.0 / n as f64
    );
    println!(
        "décodage seul (segments de l'analyse) : {:.3} s/piste",
        par_piste(t_dec)
    );
    println!(
        "Catmull-Rom 4× (ancien)             : {:.4} s/piste",
        par_piste(t_cr)
    );
    println!(
        "annexe 2, filtre Tune, avec économie : {:.4} s/piste",
        par_piste(t_fir)
    );
    println!(
        "annexe 2, filtre Tune, sans économie : {:.4} s/piste (pire cas)",
        par_piste(t_brut)
    );
    println!(
        "rattrapage (crête seule, décodage compris) : {:.3} s/piste",
        par_piste(t_rattrapage)
    );
    println!(
        "mesure complète de la passe nominale        : {:.3} s/piste",
        par_piste(t_complete)
    );
    println!(
        "rattrapage, 10 000 pistes, un fichier à la fois : {:.1} h",
        par_piste(t_rattrapage) * 10_000.0 / 3600.0
    );
    ecarts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |v: &Vec<f64>, p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    println!(
        "écart crête annexe 2 − Catmull-Rom (dB) : min {:+.3}, médiane {:+.3}, p90 {:+.3}, max {:+.3}",
        q(&ecarts, 0.0),
        q(&ecarts, 0.5),
        q(&ecarts, 0.9),
        q(&ecarts, 1.0)
    );
    println!(
        "pistes dont la crête monte de plus de 0,1 dB : {} / {n}",
        ecarts.iter().filter(|&&e| e > 0.1).count()
    );
    gains_changes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let touchees = gains_changes.iter().filter(|&&g| g.abs() > 1e-9).count();
    println!(
        "gain appliqué (prevent_clipping, 0 dBTP) : {touchees} / {n} pistes changent, \
         de {:+.3} à {:+.3} dB",
        q(&gains_changes, 0.0),
        q(&gains_changes, 1.0)
    );
}
