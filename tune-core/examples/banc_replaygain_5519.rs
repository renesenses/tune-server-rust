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

/// Une empreinte fait plusieurs Kio : on n'en journalise qu'un condensé
/// (FNV-1a 64 bits), assez pour un `cmp` entre deux versions.
fn condense(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// La pause que la boucle de `replaygain::spawn` marque entre deux tours qui
/// ont travaillé.
fn pause_entre_deux_tours(b: &Arc<dyn DbBackend>) -> std::time::Duration {
    tune_core::audio::replaygain::pause_entre_deux_tours(b)
}

/// F — LA BOUCLE RÉELLE sur une GRANDE base (#5519, mesure du 05/10).
///
/// Ni l'étage B (base de 40 pistes, `analyze_track_batch` seul) ni l'étage C
/// (requêtes sans fichier) ne disent ce que voit Tades : 528 352 pistes, dont
/// la plupart déjà analysées, et la boucle de `spawn` qui enchaîne un tour de
/// cascade, la passe d'albums et sa pause. Ici : `pistes_synth` pistes déjà
/// analysées (gains de piste et d'album, plage dynamique, empreinte), puis les
/// vrais fichiers EN QUEUE d'identifiants — c'est l'ordre d'un scan, et le
/// pire cas de la sélection, qui relit tout ce qui est déjà fait.
fn etage_f(
    rt: &tokio::runtime::Runtime,
    fichiers: &[String],
    dossier: &std::path::Path,
    pistes_synth: i64,
) {
    use tune_core::audio::replaygain::{TourDeCascade, passe_d_albums_du_tour, un_tour_de_cascade};
    let vitesses: Vec<String> = std::env::var("BANC_VITESSES")
        .unwrap_or_else(|_| "normal,fast".into())
        .split(',')
        .map(str::to_string)
        .collect();
    for vitesse in vitesses {
        let (db, b) = base_sur_disque(dossier, &format!("boucle-{vitesse}.db"));
        b.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
            &[
                &tune_core::taches_de_fond::vitesse::CLE_REGLAGE as &dyn ToSqlValue,
                &vitesse.as_str() as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        let albums_synth = pistes_synth / 12 + 1;
        let marque = format!("{}:-", tune_core::audio::empreinte::VERSION);
        let t = Instant::now();
        db.execute_batch(&format!(
            "BEGIN;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {albums_synth})
               INSERT INTO albums (id, title) SELECT i, 'a' FROM n;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {pistes_synth})
               INSERT INTO tracks (id, title, album_id, file_path, duration_ms, sample_rate, channels, format, audio_fingerprint)
               SELECT i, 't', i / 12 + 1, '/nulle/part/' || i || '.flac', 240000, 44100, 2, 'flac', '{marque}' FROM n;
             INSERT INTO track_metadata (track_id, key, value) SELECT id, 'rg_analyzed', '1' FROM tracks;
             INSERT INTO track_metadata (track_id, key, value) SELECT id, 'rg_track_gain', '-3.00 dB' FROM tracks;
             INSERT INTO track_metadata (track_id, key, value) SELECT id, 'rg_album_gain', '-3.00 dB' FROM tracks;
             INSERT INTO track_metadata (track_id, key, value) SELECT id, 'dr_track', '12' FROM tracks;
             COMMIT;"
        ))
        .expect("base synthetique");
        let premier_album = albums_synth + 1;
        for (i, f) in fichiers.iter().enumerate() {
            let id = pistes_synth + 1 + i as i64;
            let album = premier_album + i as i64 / 12;
            b.execute(
                "INSERT OR IGNORE INTO albums (id, title) VALUES (?, 'r')",
                &[&album as &dyn ToSqlValue],
            )
            .unwrap();
            b.execute(
                "INSERT INTO tracks (id, title, album_id, file_path, duration_ms, sample_rate, channels, format) \
                 VALUES (?, 't', ?, ?, 240000, 44100, 2, 'flac')",
                &[&id as &dyn ToSqlValue, &album as &dyn ToSqlValue, f as &dyn ToSqlValue],
            )
            .unwrap();
        }
        let _ = db.execute_batch("ANALYZE;");
        println!(
            "F — {vitesse:<8} base de {pistes_synth} pistes analysees + {} a faire (remplissage {:.1} s, hors mesure)",
            fichiers.len(),
            t.elapsed().as_secs_f64()
        );
        let largeur = tune_core::taches_de_fond::vitesse::largeur_courante(&b);
        let (mut t_cascade, mut t_album, mut t_pause) = (0.0, 0.0, 0.0);
        let (mut tours, mut total) = (0usize, 0usize);
        let debut = Instant::now();
        loop {
            let t0 = Instant::now();
            let tour = rt.block_on(un_tour_de_cascade(&b));
            t_cascade += t0.elapsed().as_secs_f64();
            let t1 = Instant::now();
            let albums = rt.block_on(passe_d_albums_du_tour(&b, false));
            t_album += t1.elapsed().as_secs_f64();
            let n = match tour {
                TourDeCascade::Travail(n) => n,
                _ => 0,
            };
            println!(
                "    tour {:>2} : {n:>2} pistes, {albums} album(s), cascade {:>6.2} s, albums {:>5.2} s",
                tours + 1,
                t1.duration_since(t0).as_secs_f64(),
                t1.elapsed().as_secs_f64()
            );
            if n == 0 && albums == 0 {
                break;
            }
            total += n;
            tours += 1;
            let t2 = Instant::now();
            let pause = pause_entre_deux_tours(&b);
            rt.block_on(async { tokio::time::sleep(pause).await });
            t_pause += t2.elapsed().as_secs_f64();
        }
        let mur = debut.elapsed().as_secs_f64();
        let t = Instant::now();
        let restant = tune_core::audio::replaygain::compter_les_candidats_replaygain(&b);
        let t_compte = t.elapsed().as_secs_f64();
        println!(
            "  largeur {largeur} : {total} pistes en {mur:.1} s = {:.0} pistes/h ; {tours} tours",
            total as f64 * 3600.0 / mur
        );
        println!(
            "  cascade {t_cascade:.1} s ({:.0} %), passe d'albums {t_album:.1} s ({:.0} %), pauses {t_pause:.1} s ({:.0} %)",
            100.0 * t_cascade / mur,
            100.0 * t_album / mur,
            100.0 * t_pause / mur
        );
        println!(
            "  candidats restants {restant} (compte complet : {:.0} ms)",
            t_compte * 1e3
        );
        // Ce que la passe a ÉCRIT pour les vrais fichiers, au caractère près :
        // une ligne `V` par clé (sauf l'heure du témoin `rg_analyzed`), plus
        // l'empreinte. À comparer avec `cmp` entre deux versions du code.
        let ecrit = b
            .query_many(
                "SELECT t.file_path, m.key, m.value FROM track_metadata m \
                 JOIN tracks t ON t.id = m.track_id \
                 WHERE t.id > ? AND m.key != 'rg_analyzed' ORDER BY t.file_path, m.key",
                &[&pistes_synth as &dyn ToSqlValue],
            )
            .unwrap_or_default();
        for r in &ecrit {
            let s = |i: usize| r.get(i).and_then(|v| v.as_string()).unwrap_or_default();
            eprintln!("V {vitesse} {} {} {}", s(0), s(1), s(2));
        }
        let empreintes = b
            .query_many(
                "SELECT file_path, audio_fingerprint FROM tracks WHERE id > ? ORDER BY file_path",
                &[&pistes_synth as &dyn ToSqlValue],
            )
            .unwrap_or_default();
        for r in &empreintes {
            let s = |i: usize| r.get(i).and_then(|v| v.as_string()).unwrap_or_default();
            let e = s(1);
            eprintln!("V {vitesse} {} empreinte {:x}", s(0), condense(&e));
        }
        drop(b);
        drop(db);
    }
}

/// G — LES DEUX POSTES de la suite #5519, isolés du décodage : la sélection
/// des candidats et l'écriture d'un tour, sur une base de `pistes_synth`
/// pistes déjà analysées et 25 candidates en queue d'identifiants.
///
/// Chaque mesure compare, sur la MÊME base et dans la même fenêtre de charge,
/// la forme d'avant (recopiée ici telle qu'elle était) et la forme d'après
/// (appelée par l'API publique). Sur SQLite, et sur PostgreSQL si
/// `BANC_PG_URL` est posé (la base doit être vide : le banc y crée ses
/// tables, puis les supprime).
#[cfg(feature = "postgres")]
fn base_pg(rt: &tokio::runtime::Runtime) -> Option<Arc<dyn DbBackend>> {
    let url = std::env::var("BANC_PG_URL").ok()?;
    let pool = rt
        .block_on(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .connect(&url),
        )
        .expect("connexion PostgreSQL");
    Some(Arc::new(tune_core::db::backend::PostgresBackend::new(pool)))
}

#[cfg(not(feature = "postgres"))]
fn base_pg(_rt: &tokio::runtime::Runtime) -> Option<Arc<dyn DbBackend>> {
    None
}

fn remplir_g(b: &Arc<dyn DbBackend>, pistes_synth: i64, pg: bool) {
    let serie = if pg {
        format!("SELECT i FROM generate_series(1, {pistes_synth}) AS g(i)")
    } else {
        format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {pistes_synth}) SELECT i FROM n"
        )
    };
    let reqs = [
        format!(
            "INSERT INTO tracks (id, title, file_path, duration_ms, sample_rate, channels, format, audio_fingerprint) \
             SELECT i, 't', '/nulle/part/' || i || '.flac', 240000, 44100, 2, 'flac', 'x' FROM ({serie}) s"
        ),
        "INSERT INTO track_metadata (track_id, key, value) SELECT id, 'rg_analyzed', '1' FROM tracks".into(),
        "INSERT INTO track_metadata (track_id, key, value) SELECT id, 'rg_track_gain', '-3.00 dB' FROM tracks".into(),
        "INSERT INTO track_metadata (track_id, key, value) SELECT id, 'rg_album_gain', '-3.00 dB' FROM tracks".into(),
        "INSERT INTO track_metadata (track_id, key, value) SELECT id, 'dr_track', '12' FROM tracks".into(),
    ];
    for r in &reqs {
        b.execute(r, &[]).expect("remplissage G");
    }
    for i in 1..=25i64 {
        let id = pistes_synth + i;
        let chemin = format!("/nulle/part/neuve-{i}.flac");
        b.execute(
            "INSERT INTO tracks (id, title, file_path, duration_ms, sample_rate, channels, format) \
             VALUES (?, 't', ?, 240000, 44100, 2, 'flac')",
            &[&id as &dyn ToSqlValue, &chemin as &dyn ToSqlValue],
        )
        .unwrap();
    }
    let _ = pg;
    let _ = b.execute("ANALYZE", &[]);
}

fn mediane(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn etage_g_sur(rt: &tokio::runtime::Runtime, b: &Arc<dyn DbBackend>, nom: &str, pistes_synth: i64) {
    use tune_core::audio::replaygain as rg;
    use tune_core::audio::replaygain::{
        EcrituresDePiste, ecrire_le_tour, selectionner_les_candidats_replaygain,
    };
    use tune_core::db::track_metadata_repo::TrackMetadataRepo;
    let seuil = "00000000000000000000";
    // AVANT : la requête d'avant le curseur, telle qu'elle était.
    let avant = "SELECT t.id, t.file_path, t.duration_ms, t.sample_rate, t.channels FROM tracks t \
         WHERE t.file_path IS NOT NULL AND t.file_path != '' \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_analyzed') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_track_gain') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_path_unresolved' AND m.value > ?) \
         LIMIT ?";
    let mesurer = |f: &dyn Fn() -> usize| -> (f64, usize) {
        let mut t = Vec::new();
        let mut n = 0;
        for _ in 0..7 {
            let d = Instant::now();
            n = f();
            t.push(d.elapsed().as_secs_f64() * 1e3);
        }
        (mediane(t), n)
    };
    let (t_avant, n_avant) = mesurer(&|| {
        b.query_many(
            avant,
            &[&seuil as &dyn ToSqlValue, &25i64 as &dyn ToSqlValue],
        )
        .unwrap()
        .len()
    });
    // APRÈS : un tour suivant, curseur posé sur la dernière piste déjà vue
    // (ici : la dernière déjà analysée), puis le tour complet de reprise.
    let (t_curseur, n_curseur) = mesurer(&|| {
        selectionner_les_candidats_replaygain(b, pistes_synth, 25)
            .unwrap()
            .len()
    });
    let (t_reprise, n_reprise) = mesurer(&|| {
        selectionner_les_candidats_replaygain(b, 0, 25)
            .unwrap()
            .len()
    });
    // Le curseur au-delà de la dernière piste : ce que coûte le constat « rien
    // après moi » qui précède la reprise depuis 0.
    let (t_fin, _) = mesurer(&|| {
        selectionner_les_candidats_replaygain(b, pistes_synth + 25, 25)
            .unwrap()
            .len()
    });
    println!(
        "G {nom:<6} selection : avant {t_avant:>8.2} ms ({n_avant}) | curseur {t_curseur:>6.2} ms ({n_curseur}) | \
         curseur en fin {t_fin:>6.2} ms | reprise depuis 0 {t_reprise:>8.2} ms ({n_reprise})"
    );

    // ÉCRITURES d'un tour de 25 pistes : mêmes clés, mêmes valeurs.
    let ecritures = |base: i64| -> Vec<EcrituresDePiste> {
        (1..=25i64)
            .map(|i| EcrituresDePiste {
                track_id: base + i,
                effacer_le_report: true,
                report: None,
                mesure: Some((-12.5 - i as f64 / 7.0, 0.98, 1.01, Some(11))),
                empreinte: Some(format!("env100ms-v1:{}", "ab".repeat(400))),
                temoin: Some("1790000000".into()),
            })
            .collect()
    };
    let repo = TrackMetadataRepo::with_backend(b.clone());
    let mut t_un = Vec::new();
    let mut t_groupe = Vec::new();
    for tour in 0..5 {
        // AVANT : une instruction à la fois, chacune sa transaction — la
        // séquence de l'ancienne `analyser_une_piste`.
        let lot = ecritures(pistes_synth);
        let d = Instant::now();
        for e in &lot {
            let id = e.track_id;
            let _ = repo.delete(id, "rg_path_unresolved");
            let (lufs, pk, tp, dr) = e.mesure.unwrap();
            let _ = repo.set(
                id,
                "rg_track_gain",
                &rg::format_gain(rg::track_gain_db(lufs)),
            );
            let _ = repo.set(id, "rg_track_peak", &rg::format_peak(pk));
            let _ = repo.set(id, "rg_track_true_peak", &rg::format_peak(tp));
            let _ = repo.set(id, rg::TRACK_SOURCE_KEY, rg::SOURCE_ANALYSIS);
            let existant = repo
                .get_all(id)
                .ok()
                .and_then(|m| m.get("dr_track").cloned());
            if existant.is_none() {
                let _ = repo.set(id, "dr_track", &dr.unwrap().to_string());
                let _ = repo.set(id, "dr_source", "analysis");
            }
            let _ = b.execute(
                "UPDATE tracks SET audio_fingerprint = ? WHERE id = ?",
                &[
                    &e.empreinte.as_deref().unwrap() as &dyn ToSqlValue,
                    &id as &dyn ToSqlValue,
                ],
            );
            let _ = repo.set(id, "rg_analyzed", e.temoin.as_deref().unwrap());
        }
        t_un.push(d.elapsed().as_secs_f64() * 1e3);
        // Remise à l'état « à faire » pour la forme d'après.
        b.execute(
            "DELETE FROM track_metadata WHERE track_id > ?",
            &[&pistes_synth as &dyn ToSqlValue],
        )
        .unwrap();
        let lot = ecritures(pistes_synth);
        let d = Instant::now();
        let issue = ecrire_le_tour(b, &lot);
        t_groupe.push(d.elapsed().as_secs_f64() * 1e3);
        assert_eq!(
            issue,
            tune_core::audio::replaygain::EcritureDuTour::Groupee,
            "tour {tour}"
        );
        b.execute(
            "DELETE FROM track_metadata WHERE track_id > ?",
            &[&pistes_synth as &dyn ToSqlValue],
        )
        .unwrap();
    }
    let _ = rt;
    println!(
        "G {nom:<6} ecriture d'un tour de 25 pistes : piste a piste {:>7.2} ms | groupee {:>7.2} ms (une transaction)",
        mediane(t_un),
        mediane(t_groupe)
    );
}

fn etage_g(rt: &tokio::runtime::Runtime, dossier: &std::path::Path, pistes_synth: i64) {
    let (db, b) = base_sur_disque(dossier, "selection.db");
    db.execute_batch("BEGIN").unwrap();
    remplir_g(&b, pistes_synth, false);
    db.execute_batch("COMMIT").unwrap();
    let _ = db.execute_batch("ANALYZE;");
    etage_g_sur(rt, &b, "sqlite", pistes_synth);
    for (nom, sql) in [
        (
            "avant",
            "EXPLAIN QUERY PLAN SELECT t.id FROM tracks t WHERE t.file_path IS NOT NULL AND t.file_path != '' AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_analyzed') LIMIT 25",
        ),
        (
            "apres",
            "EXPLAIN QUERY PLAN SELECT t.id FROM tracks t WHERE t.file_path IS NOT NULL AND t.file_path != '' AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_analyzed') AND t.id > 1 ORDER BY t.id LIMIT 25",
        ),
    ] {
        for r in b.query_many(sql, &[]).unwrap_or_default() {
            let l: Vec<String> = r
                .iter()
                .map(|v| v.as_string().unwrap_or_default())
                .collect();
            println!("  plan sqlite {nom} : {}", l.join(" | "));
        }
    }
    drop(b);
    drop(db);
    if let Some(pg) = base_pg(rt) {
        // Le moteur PostgreSQL appelle `Handle::current()` : entrer dans le
        // contexte du runtime, sans y exécuter de futur.
        let _contexte = rt.enter();
        for t in ["track_metadata", "tracks", "settings"] {
            let _ = pg.execute(&format!("DROP TABLE IF EXISTS {t} CASCADE"), &[]);
        }
        pg.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE tracks (id BIGINT PRIMARY KEY, title TEXT, file_path TEXT,
                duration_ms BIGINT, sample_rate BIGINT, channels BIGINT, format TEXT,
                audio_fingerprint TEXT);
             CREATE TABLE track_metadata (track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (track_id, key));
             CREATE INDEX idx_track_metadata_key ON track_metadata(key);",
        )
        .expect("schema PG du banc");
        remplir_g(&pg, pistes_synth, true);
        etage_g_sur(rt, &pg, "pg", pistes_synth);
        let predicat = "t.file_path IS NOT NULL AND t.file_path != '' \
            AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_analyzed') \
            AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_track_gain') \
            AND NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'rg_path_unresolved' AND m.value > '0')";
        for (nom, sql) in [
            (
                "avant",
                format!("EXPLAIN ANALYZE SELECT t.id FROM tracks t WHERE {predicat} LIMIT 25"),
            ),
            (
                "apres",
                format!(
                    "EXPLAIN ANALYZE SELECT t.id FROM tracks t WHERE {predicat} AND t.id > {pistes_synth} ORDER BY t.id LIMIT 25"
                ),
            ),
            (
                "reprise",
                format!(
                    "EXPLAIN ANALYZE SELECT t.id FROM tracks t WHERE {predicat} AND t.id > 0 ORDER BY t.id LIMIT 25"
                ),
            ),
        ] {
            for r in pg.query_many(&sql, &[]).unwrap_or_default() {
                let l: Vec<String> = r
                    .iter()
                    .map(|v| v.as_string().unwrap_or_default())
                    .collect();
                println!("  plan pg {nom} : {}", l.join(" | "));
            }
        }
        for t in ["track_metadata", "tracks", "settings"] {
            let _ = pg.execute(&format!("DROP TABLE IF EXISTS {t} CASCADE"), &[]);
        }
    }
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
    if std::env::var("BANC_SEUL_G").is_ok() {
        etage_g(&rt, tmp.path(), pistes_synth);
        return;
    }
    if std::env::var("BANC_SEUL_F").is_ok() {
        etage_f(&rt, &fichiers, tmp.path(), pistes_synth);
        return;
    }
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
