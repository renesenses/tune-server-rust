//! #8609 — `malloc_trim` pendant une lecture LOCALE fait-il attendre le rendu ?
//!
//! Banc de MESURE (`#[ignore]`, Linux + glibc), pas une garde de CI :
//!
//! ```text
//! cargo test -p tune-core --lib banc_trim_pendant_lecture_8609 -- --ignored --nocapture
//! ```
//!
//! Rien n'est joué : pas de cpal, pas de carte son. Le rappel est
//! `render_local_shared_f32_callback` — le corps RÉEL des flux cpal partagés —
//! appelé période par période (1 024 échantillons, 11,6 ms à 44,1 kHz) par un
//! fil qui tient l'horloge, comme le ferait le fil ALSA de cpal ; l'anneau est
//! le `RingBuf` de production (deux secondes), nourri par un producteur qui
//! ALLOUE un tampon par paquet, comme un décodeur.
//!
//! Autour : la passe ReplayGain RÉELLE (`mesurer_intensite_et_plage`) sur des
//! WAV 32 bits / 96 kHz écrits pour l'occasion, et deux fils qui allouent et
//! libèrent des blocs de 16 Kio à 2 Mio en rafale (la signature d'une session
//! ONNX), en gardant de petits bouchons qui fragmentent leurs arènes.
//!
//! Trois phases de même durée (`BANC_8609_S`, 20 s par défaut) :
//! 1. sans purge ;
//! 2. purge toutes les 500 ms (600 fois le rythme du relevé de production) ;
//! 3. contre-épreuve : même purge, mais un rappel qui ALLOUE à chaque période
//!    — ce que le rendu ne fait plus depuis #2218. C'est lui, et lui seul, que
//!    la purge pourrait faire attendre.
//!
//! Chiffres relevés : durée du rappel (max, p99,9), retard de réveil, périodes
//! en famine, remplissage minimal de l'anneau, durée des purges, RSS et
//! `mallinfo2` avant / après une purge.

use super::*;
use std::time::{Duration, Instant};

const TAUX: u32 = 44_100;
const CANAUX: u16 = 2;
const PERIODE: usize = 1_024;
const PERIODE_US: u64 = 11_610;

#[derive(Debug, Default)]
struct MesureDuRendu {
    periodes: usize,
    famines: usize,
    rappel_max_us: u64,
    rappel_p999_us: u64,
    retard_max_us: u64,
}

struct MesureDuProducteur {
    remplissage_min_ms: u64,
}

fn ecrire_wav_32(chemin: &std::path::Path, secondes: u32, graine: u32) {
    use std::io::Write;
    let taux = 96_000u32;
    let canaux = 2u16;
    let n = (taux * secondes) as usize * canaux as usize;
    let octets = (n * 4) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(chemin).unwrap());
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + octets).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&canaux.to_le_bytes()).unwrap();
    f.write_all(&taux.to_le_bytes()).unwrap();
    f.write_all(&(taux * canaux as u32 * 4).to_le_bytes())
        .unwrap();
    f.write_all(&(canaux * 4).to_le_bytes()).unwrap();
    f.write_all(&32u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&octets.to_le_bytes()).unwrap();
    let mut x = graine.wrapping_mul(2_654_435_761).max(1);
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let t = (i / 2) as f64 / taux as f64;
        let s = 0.4 * (2.0 * std::f64::consts::PI * (220.0 + graine as f64) * t).sin()
            + 0.05 * ((x as f64 / u32::MAX as f64) - 0.5);
        f.write_all(&((s * i32::MAX as f64) as i32).to_le_bytes())
            .unwrap();
    }
}

/// Le fil de rendu : appelle le rappel réel à chaque échéance de période.
fn rendre(
    ring: Arc<RingBuf>,
    arret: Arc<AtomicBool>,
    allouer_dans_le_rappel: bool,
    attendu: usize,
) -> MesureDuRendu {
    let volume = AtomicU32::new(UNITE_DU_GAIN_DE_RENDU);
    let paused = AtomicBool::new(false);
    let silent = AtomicBool::new(false);
    let started = AtomicBool::new(true);
    let mut ramp = crate::audio::soft_mute::SoftMuteRamp::new(TAUX, CANAUX);
    let mut sortie = vec![0.0f32; PERIODE];
    // Fait AVANT la boucle : la boucle elle-même n'alloue pas.
    let mut durees: Vec<u64> = Vec::with_capacity(attendu + 1_000);
    let mut m = MesureDuRendu::default();
    let periode = Duration::from_micros(PERIODE_US);
    let mut echeance = Instant::now() + periode;
    while !arret.load(Ordering::Relaxed) {
        let maintenant = Instant::now();
        if maintenant < echeance {
            std::thread::sleep(echeance - maintenant);
        }
        let t0 = Instant::now();
        m.retard_max_us = m
            .retard_max_us
            .max(t0.saturating_duration_since(echeance).as_micros() as u64);
        let lu = if allouer_dans_le_rappel {
            let mut tampon = vec![0.0f32; PERIODE];
            let lu = render_local_shared_f32_callback(
                &ring,
                &volume,
                &paused,
                &silent,
                &started,
                &mut ramp,
                0,
                0,
                &mut tampon,
            );
            sortie.copy_from_slice(&tampon);
            lu
        } else {
            render_local_shared_f32_callback(
                &ring,
                &volume,
                &paused,
                &silent,
                &started,
                &mut ramp,
                0,
                0,
                &mut sortie,
            )
        };
        let d = t0.elapsed().as_micros() as u64;
        if durees.len() < durees.capacity() {
            durees.push(d);
        }
        m.periodes += 1;
        if lu < PERIODE {
            m.famines += 1;
        }
        echeance += periode;
    }
    durees.sort_unstable();
    m.rappel_max_us = durees.last().copied().unwrap_or(0);
    if !durees.is_empty() {
        m.rappel_p999_us = durees[(durees.len() * 999 / 1000).min(durees.len() - 1)];
    }
    m
}

/// Le producteur : un tampon ALLOUÉ par paquet, comme un décodeur.
fn produire(ring: Arc<RingBuf>, arret: Arc<AtomicBool>, capacite: usize) -> MesureDuProducteur {
    const PAQUET: usize = 4_096;
    let mut min = usize::MAX;
    let mut amorce = false;
    while !arret.load(Ordering::Relaxed) {
        let dispo = ring.available();
        if amorce {
            min = min.min(dispo);
        }
        if dispo + PAQUET <= capacite {
            let paquet: Vec<f32> = (0..PAQUET)
                .map(|i| ((i % 64) as f32 / 64.0) - 0.5)
                .collect();
            ring.push(&paquet);
        } else {
            amorce = true;
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let ech_par_ms = (TAUX as usize * CANAUX as usize) / 1000;
    MesureDuProducteur {
        remplissage_min_ms: if min == usize::MAX {
            0
        } else {
            (min / ech_par_ms) as u64
        },
    }
}

/// Rafales d'allocations type session ONNX, avec bouchons qui fragmentent.
/// Rend ses bouchons : ils restent vivants le temps du relevé, comme ceux
/// d'un fil du pool `spawn_blocking` qui survit à sa tâche.
fn baratter(arret: Arc<AtomicBool>, graine: u64) -> Vec<Vec<u8>> {
    let mut x = graine.max(1);
    let mut bouchons: Vec<Vec<u8>> = Vec::new();
    while !arret.load(Ordering::Relaxed) {
        let mut blocs = Vec::with_capacity(200);
        for _ in 0..200 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let taille = 16 * 1024 + (x % (2 * 1024 * 1024)) as usize;
            let mut b = vec![0u8; taille];
            b[taille / 2] = 1;
            blocs.push(b);
        }
        bouchons.push(vec![3u8; 256]);
        if bouchons.len() > 2_000 {
            bouchons.drain(..1_000);
        }
        drop(blocs);
    }
    bouchons
}

fn passe_replaygain(fichiers: Vec<std::path::PathBuf>, arret: Arc<AtomicBool>) -> usize {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut n = 0;
        while !arret.load(Ordering::Relaxed) {
            for paire in fichiers.chunks(2) {
                if arret.load(Ordering::Relaxed) {
                    break;
                }
                let taches: Vec<_> = paire
                    .iter()
                    .map(|f| {
                        let f = f.to_string_lossy().into_owned();
                        tokio::spawn(async move {
                            crate::audio::analyzer::mesurer_intensite_et_plage(&f).await
                        })
                    })
                    .collect();
                for t in taches {
                    assert!(
                        t.await.unwrap().is_some(),
                        "la mesure ReplayGain doit aboutir"
                    );
                    n += 1;
                }
            }
        }
        n
    })
}

struct Phase {
    nom: &'static str,
    rendu: MesureDuRendu,
    producteur: MesureDuProducteur,
    purges: usize,
    purge_max_ms: u64,
    purge_moy_ms: f64,
    pistes_rg: usize,
    rss_max_mb: u64,
    libre_max_mb: u64,
}

fn phase(
    nom: &'static str,
    secondes: u64,
    fichiers: &[std::path::PathBuf],
    purger: Option<fn()>,
    allouer_dans_le_rappel: bool,
) -> (Phase, Vec<Vec<Vec<u8>>>) {
    let capacite = TAUX as usize * CANAUX as usize * 2;
    let famine = Arc::new(RingStarvation::new());
    famine.begin_stream(TAUX, CANAUX);
    let ring = Arc::new(RingBuf::new_metered(capacite, famine));
    let arret = Arc::new(AtomicBool::new(false));

    let prod = {
        let (r, a) = (ring.clone(), arret.clone());
        std::thread::spawn(move || produire(r, a, capacite))
    };
    // Anneau plein avant le premier rappel, comme au démarrage d'une piste.
    while ring.available() + 4_096 <= capacite {
        std::thread::sleep(Duration::from_millis(5));
    }
    let attendu = (secondes * 1_000_000 / PERIODE_US) as usize;
    let rendu = {
        let (r, a) = (ring.clone(), arret.clone());
        std::thread::Builder::new()
            .name("rendu-factice".into())
            .spawn(move || rendre(r, a, allouer_dans_le_rappel, attendu))
            .unwrap()
    };
    let rg = {
        let (f, a) = (fichiers.to_vec(), arret.clone());
        std::thread::spawn(move || passe_replaygain(f, a))
    };
    let barattes: Vec<_> = (0..2u64)
        .map(|i| {
            let a = arret.clone();
            std::thread::spawn(move || baratter(a, 0x9E37_79B9 + i))
        })
        .collect();

    let fin = Instant::now() + Duration::from_secs(secondes);
    let mut durees_purge = Vec::new();
    let (mut rss_max_mb, mut libre_max_mb) = (0u64, 0u64);
    while Instant::now() < fin {
        std::thread::sleep(Duration::from_millis(500));
        rss_max_mb = rss_max_mb.max(crate::memoire_rendue::rss_mb().unwrap_or(0));
        libre_max_mb = libre_max_mb.max(crate::memoire_rendue::etat_du_tas().unwrap().libre_mb);
        if let Some(purge) = purger {
            let t = Instant::now();
            purge();
            durees_purge.push(t.elapsed().as_micros() as u64);
        }
    }
    arret.store(true, Ordering::Relaxed);
    let rendu = rendu.join().unwrap();
    let producteur = prod.join().unwrap();
    let pistes_rg = rg.join().unwrap();
    let bouchons: Vec<Vec<Vec<u8>>> = barattes.into_iter().map(|b| b.join().unwrap()).collect();
    let purges = durees_purge.len();
    let mesure = Phase {
        nom,
        rendu,
        producteur,
        purges,
        purge_max_ms: durees_purge.iter().max().copied().unwrap_or(0) / 1000,
        purge_moy_ms: if purges == 0 {
            0.0
        } else {
            durees_purge.iter().sum::<u64>() as f64 / purges as f64 / 1000.0
        },
        pistes_rg,
        rss_max_mb,
        libre_max_mb,
    };
    (mesure, bouchons)
}

fn releve(quand: &str) {
    let rss = crate::memoire_rendue::rss_mb().unwrap_or(0);
    let tas = crate::memoire_rendue::etat_du_tas().unwrap();
    println!(
        "  {quand:<34} RSS {rss:>5} Mo | tas en usage {:>5} Mo, libre gardé {:>5} Mo, mmap {:>4} Mo",
        tas.en_usage_mb, tas.libre_mb, tas.mmap_mb
    );
}

fn purge_simple() {
    crate::memoire_rendue::rendre_la_memoire_liberee();
}

/// Variante évaluée pour Tune OS (#8609, point 2 de la relecture) : lever le
/// verrouillage le temps de la purge, puis le reposer à l'identique.
fn purge_avec_reverrouillage() {
    // SAFETY : appels sans pointeur.
    unsafe { libc::munlockall() };
    crate::memoire_rendue::rendre_la_memoire_liberee();
    unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT) };
}

fn preparer_les_pistes() -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    let dossier = std::env::temp_dir().join(format!("banc-8609-{}", std::process::id()));
    std::fs::create_dir_all(&dossier).unwrap();
    let fichiers = (0..4u32)
        .map(|i| {
            let f = dossier.join(format!("piste-{i}.wav"));
            ecrire_wav_32(&f, 60, i + 1);
            f
        })
        .collect();
    (dossier, fichiers)
}

fn secondes_par_phase() -> u64 {
    std::env::var("BANC_8609_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}

fn imprimer(phases: &[Phase]) {
    println!(
        "\n  {:<28} {:>8} {:>7} {:>10} {:>8} {:>10} {:>10} {:>6} {:>9} {:>9} {:>3} {:>8} {:>9}",
        "phase",
        "périodes",
        "famines",
        "rappel_max",
        "p99,9",
        "réveil_max",
        "anneau_min",
        "purges",
        "purge_max",
        "purge_moy",
        "RG",
        "RSS_max",
        "libre_max"
    );
    for p in phases {
        println!(
            "  {:<28} {:>8} {:>7} {:>8}µs {:>6}µs {:>8}µs {:>8}ms {:>6} {:>7}ms {:>7.1}ms {:>3} {:>6}Mo {:>7}Mo",
            p.nom,
            p.rendu.periodes,
            p.rendu.famines,
            p.rendu.rappel_max_us,
            p.rendu.rappel_p999_us,
            p.rendu.retard_max_us,
            p.producteur.remplissage_min_ms,
            p.purges,
            p.purge_max_ms,
            p.purge_moy_ms,
            p.pistes_rg,
            p.rss_max_mb,
            p.libre_max_mb
        );
    }
}

#[test]
#[ignore = "banc de mesure #8609 : lancer à la main sur Shrek"]
fn banc_trim_pendant_lecture_8609() {
    let seuil = crate::memoire_rendue::fixer_le_seuil_mmap();
    let secondes = secondes_par_phase();
    let (dossier, fichiers) = preparer_les_pistes();
    println!(
        "\n#8609 — banc malloc_trim pendant lecture locale ({secondes} s par phase, seuil mmap : {})",
        seuil.libelle()
    );
    println!(
        "  MALLOC_ARENA_MAX={}",
        std::env::var("MALLOC_ARENA_MAX").unwrap_or_else(|_| "(défaut)".into())
    );
    releve("départ");

    let mut phases = Vec::new();
    let (p, bouchons) = phase("1. sans purge", secondes, &fichiers, None, false);
    phases.push(p);
    releve("fin phase 1 (aucune purge)");
    let t = Instant::now();
    crate::memoire_rendue::rendre_la_memoire_liberee();
    println!("  une purge : {} ms", t.elapsed().as_millis());
    releve("après UNE purge");
    drop(bouchons);
    purge_simple();
    let (p, bouchons) = phase(
        "2. purge /500 ms",
        secondes,
        &fichiers,
        Some(purge_simple),
        false,
    );
    phases.push(p);
    releve("fin phase 2");
    drop(bouchons);
    purge_simple();
    let (p, bouchons) = phase(
        "3. contre-épreuve : alloue",
        secondes,
        &fichiers,
        Some(purge_simple),
        true,
    );
    phases.push(p);
    releve("fin phase 3");
    drop(bouchons);
    imprimer(&phases);
    let _ = std::fs::remove_dir_all(&dossier);
}

/// Point 2 de la relecture de #8614 : sous `mlockall` (Tune OS et `.deb`
/// posent `LimitMEMLOCK=infinity`, et `audio::ordonnancement_rt` verrouille
/// alors `MCL_CURRENT|MCL_FUTURE|MCL_ONFAULT`), que rend la purge, et que
/// coûtent les variantes ?
///
/// `BANC_8609_MLOCK` choisit le verrouillage posé AU DÉPART du processus :
/// - `future` : celui de production ; purge simple ;
/// - `relock` : celui de production ; purge = `munlockall`, `malloc_trim`,
///   `mlockall` identique ;
/// - `courant` : `MCL_CURRENT|MCL_ONFAULT` seul (ce qui sera alloué ensuite
///   — ici l'anneau et les tampons de la phase — n'est PAS verrouillé) ;
/// - `aucun` : témoin.
///
/// Demande `RLIMIT_MEMLOCK` assez grande (Shrek : 31 Gio) ; en production,
/// une limite finie écarte `mlockall` (`decider_le_verrouillage`).
#[test]
#[ignore = "banc de mesure #8609 : lancer à la main sur Shrek"]
fn banc_trim_sous_mlockall_8609() {
    crate::memoire_rendue::fixer_le_seuil_mmap();
    let mode = std::env::var("BANC_8609_MLOCK").unwrap_or_else(|_| "future".into());
    let secondes = secondes_par_phase();
    let (dossier, fichiers) = preparer_les_pistes();
    let drapeaux = match mode.as_str() {
        "future" | "relock" => Some(libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT),
        "courant" => Some(libc::MCL_CURRENT | libc::MCL_ONFAULT),
        _ => None,
    };
    if let Some(d) = drapeaux {
        // SAFETY : appel sans pointeur.
        let rc = unsafe { libc::mlockall(d) };
        assert_eq!(
            rc,
            0,
            "mlockall refusé : {}",
            std::io::Error::last_os_error()
        );
    }
    let purge: fn() = if mode == "relock" {
        purge_avec_reverrouillage
    } else {
        purge_simple
    };
    println!("\n#8609 — banc sous mlockall : mode {mode} ({secondes} s par phase)");
    let vmlck = || crate::memoire_rendue::memoire_verrouillee_kb().unwrap_or(0) / 1024;
    releve("départ");
    let mut phases = Vec::new();
    let (p, bouchons) = phase("1. sans purge", secondes, &fichiers, None, false);
    phases.push(p);
    releve("fin phase 1 (aucune purge)");
    println!("  VmLck {} Mo", vmlck());
    let t = Instant::now();
    purge();
    println!("  une purge ({mode}) : {} ms", t.elapsed().as_millis());
    releve("après UNE purge");
    println!("  VmLck {} Mo", vmlck());
    drop(bouchons);
    purge();
    let (p, bouchons) = phase("2. purge /500 ms", secondes, &fichiers, Some(purge), false);
    phases.push(p);
    releve("fin phase 2");
    let t = Instant::now();
    purge();
    println!("  une purge ({mode}) : {} ms", t.elapsed().as_millis());
    releve("après UNE purge");
    println!("  VmLck {} Mo", vmlck());
    drop(bouchons);
    imprimer(&phases);
    // SAFETY : appel sans pointeur.
    unsafe { libc::munlockall() };
    let _ = std::fs::remove_dir_all(&dossier);
}
