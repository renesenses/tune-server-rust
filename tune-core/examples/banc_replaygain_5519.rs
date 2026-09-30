//! Banc de débit de la passe ReplayGain de fond (#5519).
//!
//! Tades mesure ~1 300 pistes/h sur 400 000. Ce binaire dit OÙ part le temps,
//! sur de vrais fichiers, et ce que rapporte chaque vitesse :
//!
//! - **A — étages par fichier** : décodage seul (mêmes segments de 30 s que
//!   l'analyse), mesure complète (`mesurer_intensite_et_plage`), empreinte
//!   (`empreinte_du_fichier`), et la mesure PARTAGÉE
//!   (`mesurer_intensite_plage_et_empreinte`), comparée au bit près aux deux
//!   appels séparés ;
//! - **B — passe réelle** : `analyze_track_batch` sur une base SQLite sur
//!   disque (migrations complètes), jusqu'à épuisement, pour CHAQUE vitesse
//!   (`discreet`, `normal`, `fast`) ;
//! - **C — requêtes sur une grande bibliothèque** : base synthétique de
//!   `pistes_synthetiques` lignes (0 : sauté) ;
//! - **E — préfixe et contiguïté des décodages de tête**.
//!
//! ```text
//! cargo run --release -p tune-core --example banc_replaygain_5519 -- <dossier> [max_fichiers] [pistes_synthetiques]
//! ```
//!
//! La sortie d'erreur porte une ligne `R <fichier> <mesure>` par fichier : la
//! valeur exacte (`Debug` d'un `f64` fait l'aller-retour sans perte), à
//! comparer avec `cmp` entre deux versions du code.

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

type Mesure = Option<(f64, f64, f64, Option<u32>)>;

fn bits(m: Mesure) -> Option<(u64, u64, u64, Option<u32>)> {
    m.map(|(a, b, c, d)| (a.to_bits(), b.to_bits(), c.to_bits(), d))
}

fn etage_a(rt: &tokio::runtime::Runtime, fichiers: &[String]) {
    let (mut t_dec, mut t_mes, mut t_emp, mut t_part, mut audio) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut identiques = 0usize;
    for f in fichiers {
        let t = Instant::now();
        let d = decoder_seul(f);
        t_dec += t.elapsed().as_secs_f64();
        audio += d;
        let t = Instant::now();
        let m = rt.block_on(tune_core::audio::analyzer::mesurer_intensite_et_plage(f));
        t_mes += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let emp = tune_core::audio::empreinte::empreinte_du_fichier(f);
        t_emp += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let p = rt.block_on(tune_core::audio::analyzer::mesurer_intensite_plage_et_empreinte(f));
        t_part += t.elapsed().as_secs_f64();
        eprintln!("R {f} {m:?}");
        if bits(p.mesure) == bits(m) && p.empreinte.as_ref() == Some(&emp) {
            identiques += 1;
        } else {
            eprintln!("DIFF {f} partagee={:?} separee={m:?}", p.mesure);
        }
    }
    let n = fichiers.len().max(1) as f64;
    println!(
        "A — {n} fichiers, {audio:.0} s d'audio ({:.0} s/piste)",
        audio / n
    );
    println!(
        "  decodage seul        {:>7.3} s/piste  {:>6.0} xRT",
        t_dec / n,
        audio / t_dec
    );
    println!(
        "  mesure complete      {:>7.3} s/piste  {:>6.0} xRT",
        t_mes / n,
        audio / t_mes
    );
    println!("  empreinte            {:>7.3} s/piste", t_emp / n);
    println!(
        "  mesure + empreinte : separees {:.3} s/piste, partagees {:.3} s/piste ; identiques au bit pres : {identiques}/{}",
        (t_mes + t_emp) / n,
        t_part / n,
        fichiers.len()
    );
}

fn etage_b(rt: &tokio::runtime::Runtime, fichiers: &[String], dossier: &std::path::Path) {
    for vitesse in ["discreet", "normal", "fast"] {
        let (_db, b) = base_sur_disque(dossier, &format!("passe-{vitesse}.db"));
        b.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
            &[
                &tune_core::taches_de_fond::vitesse::CLE_REGLAGE as &dyn ToSqlValue,
                &vitesse as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        for (i, f) in fichiers.iter().enumerate() {
            let id = (i + 1) as i64;
            b.execute(
                "INSERT INTO tracks (id, title, file_path, duration_ms, sample_rate, channels, format) \
                 VALUES (?, 't', ?, 240000, 44100, 2, 'flac')",
                &[&id as &dyn ToSqlValue, f as &dyn ToSqlValue],
            )
            .unwrap();
        }
        let largeur = tune_core::taches_de_fond::vitesse::largeur_courante(&b);
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
            "B — {vitesse:<8} ({largeur} a la fois) : {total} pistes en {mur:.1} s = {:.3} s/piste = {:.0} pistes/h",
            mur / total.max(1) as f64,
            total as f64 * 3600.0 / mur
        );
    }
}

fn etage_c(rt: &tokio::runtime::Runtime, dossier: &std::path::Path, pistes_synth: i64) {
    let (db, b) = base_sur_disque(dossier, "grande.db");
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
            let cle = if id % 2 == 0 {
                "dr_track"
            } else {
                "dr_indisponible"
            };
            b.execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, '12')",
                &[&id as &dyn ToSqlValue, &cle as &dyn ToSqlValue],
            )
            .unwrap();
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
    chrono("analyze_album_batch (tous albums a faire)", || {
        tune_core::audio::replaygain::analyze_album_batch(&b)
    });
    chrono("compter_les_candidats_dr", || {
        tune_core::audio::replaygain::compter_les_candidats_dr(&b)
    });
}

fn etage_e(fichiers: &[String]) {
    // Le premier segment d'analyse (30 s) est-il le PRÉFIXE exact d'un
    // décodage de 90 s ? Le segment suivant (seek 30 s) en est-il la SUITE ?
    let (mut prefixes, mut suites) = (0, 0);
    for f in fichiers {
        let plein = tune_core::audio::decode::decode_to_pcm(f, None, None, 0.0, 90.0);
        let s0 = tune_core::audio::decode::decode_to_pcm(f, None, None, 0.0, 30.0);
        let s1 = tune_core::audio::decode::decode_to_pcm(f, None, None, 30.0, 30.0);
        if let (Ok(p), Ok(s0), Ok(s1)) = (plein, s0, s1) {
            let n0 = s0.samples_i32.len();
            if p.samples_i32.get(..n0) == Some(&s0.samples_i32[..]) {
                prefixes += 1;
            }
            let k = s1
                .samples_i32
                .len()
                .min(p.samples_i32.len().saturating_sub(n0));
            if k > 0 && p.samples_i32[n0..n0 + k] == s1.samples_i32[..k] {
                suites += 1;
            }
        }
    }
    println!(
        "E — premier segment = prefixe exact : {prefixes}/{} ; segment suivant = suite exacte : {suites}/{}",
        fichiers.len(),
        fichiers.len()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dossier = std::path::PathBuf::from(args.get(1).expect("dossier"));
    let max: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let pistes_synth: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let fichiers = lister(&dossier, max);
    println!("fichiers: {}", fichiers.len());
    let tmp = tempfile::TempDir::new().unwrap();
    if !fichiers.is_empty() {
        etage_a(&rt, &fichiers);
        etage_e(&fichiers);
        if std::env::var("BANC_SEUL_A").is_err() {
            etage_b(&rt, &fichiers, tmp.path());
        }
    }
    if pistes_synth > 0 {
        etage_c(&rt, tmp.path(), pistes_synth);
    }
}
