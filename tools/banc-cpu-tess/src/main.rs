//! TESS T18 — banc CPU de la chaîne type, F32 contre F64.
//!
//! ```text
//! banc-cpu-tess [--secondes 60] [--temps-reel 1800] [--commit <sha>] [--sortie rapport.md]
//! ```
//!
//! Chaîne type du plan (§ 1.1, critère CPU) : 96 kHz stéréo, égaliseur
//! graphique 31 bandes, convolution de 65 536 points, crossfeed,
//! suréchantillonnage × 2. Le MÊME algorithme tourne en `f32` puis en `f64`
//! ([`etages`]), par blocs de 1 024 trames.
//!
//! - **Débit** : `--secondes` de signal traitées aussi vite que possible, sur
//!   un seul fil ; la charge est le temps de calcul rapporté à la durée du
//!   signal, en % d'un cœur, étage par étage.
//! - **Temps réel** (`--temps-reel`, F64 seulement) : la chaîne est cadencée
//!   comme une sortie audio (un bloc toutes les 1 024 / 96 000 s) ; le banc
//!   compte les blocs rendus après leur échéance (sous-débits) et la pire
//!   avance restante. Le plan demande 30 minutes (`--temps-reel 1800`).
//! - **Rapport daté** : date, commande, commit, modèle de machine, processeur,
//!   noyau, température avant et après, en Markdown (et `--sortie`).
//!
//! Aucun son n'est joué : rien n'est envoyé à une carte son.

mod etages;

use etages::{Chaine, ETAGES, Ech};
use std::time::{Duration, Instant};

const FS: f64 = 96_000.0;
const BLOC: usize = 1024;
const LONGUEUR_RI: usize = 65_536;
const COEFFICIENTS_SURECH: usize = 64;

fn lire(chemin: &str) -> Option<String> {
    std::fs::read_to_string(chemin)
        .ok()
        .map(|s| s.trim_matches(char::from(0)).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn machine() -> String {
    lire("/proc/device-tree/model")
        .or_else(|| lire("/sys/devices/virtual/dmi/id/product_name"))
        .unwrap_or_else(|| "inconnu".into())
}

fn processeur() -> String {
    let info = lire("/proc/cpuinfo").unwrap_or_default();
    let modele = info
        .lines()
        .find(|l| l.starts_with("model name") || l.starts_with("Model"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string());
    let coeurs = info.lines().filter(|l| l.starts_with("processor")).count();
    format!(
        "{} ({} cœurs logiques, {})",
        modele.unwrap_or_else(|| std::env::consts::ARCH.into()),
        coeurs,
        std::env::consts::ARCH
    )
}

fn temperature() -> String {
    lire("/sys/class/thermal/thermal_zone0/temp")
        .and_then(|t| t.parse::<f64>().ok())
        .map_or("non lue".into(), |m| format!("{:.1} °C", m / 1000.0))
}

fn noyau() -> String {
    lire("/proc/sys/kernel/osrelease").unwrap_or_else(|| std::env::consts::OS.into())
}

fn date() -> String {
    std::process::Command::new("date")
        .arg("-u")
        .arg("+%Y-%m-%d %H:%M:%S UTC")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "inconnue".into())
}

/// Signal d'entrée : bruit déterministe à −20 dBFS environ, stéréo (jamais
/// de silence : pas de nombres dénormaux qui fausseraient le F32).
fn entree(trames: usize) -> Vec<f64> {
    let mut g = 0x9e37_79b9_7f4a_7c15_u64;
    (0..trames * 2)
        .map(|_| {
            g ^= g << 13;
            g ^= g >> 7;
            g ^= g << 17;
            ((g >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 0.2
        })
        .collect()
}

struct Mesure {
    par_etage_ns: [u64; 4],
    total_ns: u64,
    sortie: Vec<f64>,
}

fn debit<T: Ech>(signal: &[f64]) -> Mesure {
    let mut chaine = Chaine::<T>::new(FS, LONGUEUR_RI, BLOC, COEFFICIENTS_SURECH);
    let mut bloc = vec![T::default(); BLOC * 2];
    let mut par_etage_ns = [0u64; 4];
    let mut sortie = Vec::with_capacity(signal.len() * 2);
    let debut = Instant::now();
    for morceau in signal.as_chunks::<{ BLOC * 2 }>().0 {
        for (b, &x) in bloc.iter_mut().zip(morceau) {
            *b = T::de(x);
        }
        let t = chaine.traiter(&mut bloc);
        for (a, b) in par_etage_ns.iter_mut().zip(t) {
            *a += b;
        }
        sortie.extend(chaine.sortie.iter().map(|v| v.en_f64()));
    }
    Mesure {
        par_etage_ns,
        total_ns: debut.elapsed().as_nanos() as u64,
        sortie,
    }
}

struct TempsReel {
    blocs: u64,
    sous_debits: u64,
    pire_retard_us: f64,
    pire_calcul_us: f64,
    charge: f64,
}

fn temps_reel(secondes: f64) -> TempsReel {
    let mut chaine = Chaine::<f64>::new(FS, LONGUEUR_RI, BLOC, COEFFICIENTS_SURECH);
    let periode = Duration::from_secs_f64(BLOC as f64 / FS);
    let source = entree(BLOC * 64);
    let mut bloc = vec![0.0; BLOC * 2];
    let blocs = (secondes * FS / BLOC as f64).ceil() as u64;
    let (mut sous_debits, mut pire_retard, mut pire_calcul, mut calcul_total) =
        (0u64, Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let depart = Instant::now();
    for i in 0..blocs {
        let k = (i as usize % 64) * BLOC * 2;
        bloc.copy_from_slice(&source[k..k + BLOC * 2]);
        let t0 = Instant::now();
        chaine.traiter(&mut bloc);
        let fin = Instant::now();
        let calcul = fin - t0;
        calcul_total += calcul;
        pire_calcul = pire_calcul.max(calcul);
        let echeance = depart + periode * (i as u32 + 1);
        if fin > echeance {
            sous_debits += 1;
            pire_retard = pire_retard.max(fin - echeance);
        } else {
            std::thread::sleep(echeance - fin);
        }
    }
    TempsReel {
        blocs,
        sous_debits,
        pire_retard_us: pire_retard.as_secs_f64() * 1e6,
        pire_calcul_us: pire_calcul.as_secs_f64() * 1e6,
        charge: calcul_total.as_secs_f64() / (blocs as f64 * periode.as_secs_f64()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let valeur = |nom: &str| {
        args.iter()
            .position(|a| a == nom)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let secondes: f64 = valeur("--secondes")
        .and_then(|s| s.parse().ok())
        .unwrap_or(60.0);
    let tr: Option<f64> = valeur("--temps-reel").and_then(|s| s.parse().ok());
    let commit = valeur("--commit").unwrap_or_else(|| "non fourni".into());

    let mut r = String::new();
    let mut ligne = |s: String| {
        println!("{s}");
        r.push_str(&s);
        r.push('\n');
    };
    ligne("# Banc CPU TESS (T18) — chaîne type, F32 contre F64\n".into());
    ligne(format!("- Date : {}", date()));
    ligne(format!("- Commande : `{}`", args.join(" ")));
    ligne(format!("- Commit : {commit}"));
    ligne(format!("- Machine : {}", machine()));
    ligne(format!("- Processeur : {}", processeur()));
    ligne(format!("- Noyau : {}", noyau()));
    ligne(format!("- Température avant : {}", temperature()));
    ligne(format!(
        "- Chaîne : {} Hz stéréo, blocs de {BLOC} trames ; égaliseur 31 bandes (±3 dB) → convolution {LONGUEUR_RI} points ({} partitions de {BLOC}) → crossfeed → suréchantillonnage × 2 ({} coefficients par phase)",
        FS,
        LONGUEUR_RI / BLOC,
        COEFFICIENTS_SURECH
    ));
    ligne(format!(
        "- Signal : {secondes} s de bruit à −20 dBFS, un seul fil\n"
    ));

    let signal = entree((secondes * FS) as usize / BLOC * BLOC);
    let duree_ns = (signal.len() / 2) as f64 / FS * 1e9;
    let m32 = debit::<f32>(&signal);
    let m64 = debit::<f64>(&signal);

    ligne("| Étage | F32 (% d'un cœur) | F64 (% d'un cœur) | F64 / F32 |".into());
    ligne("|---|---|---|---|".into());
    for (i, nom) in ETAGES.iter().enumerate() {
        let a = m32.par_etage_ns[i] as f64 / duree_ns * 100.0;
        let b = m64.par_etage_ns[i] as f64 / duree_ns * 100.0;
        ligne(format!("| {nom} | {a:.2} | {b:.2} | {:.2} |", b / a));
    }
    let a = m32.total_ns as f64 / duree_ns * 100.0;
    let b = m64.total_ns as f64 / duree_ns * 100.0;
    ligne(format!(
        "| **Chaîne complète** | **{a:.2}** | **{b:.2}** | **{:.2}** |",
        b / a
    ));

    // Écart F32 / F64 en sortie : la précision que coûte le F32.
    let (mut num, mut den) = (0.0f64, 0.0f64);
    let mut pire = 0.0f64;
    for (x, y) in m32.sortie.iter().zip(&m64.sortie) {
        num += (x - y) * (x - y);
        den += y * y;
        pire = pire.max((x - y).abs());
    }
    ligne(format!(
        "\nÉcart F32 − F64 en sortie : {:.1} dB sous le signal (RMS), écart maximal {pire:.2e} ({:.1} dBFS).",
        10.0 * (num / den).log10(),
        20.0 * pire.log10()
    ));
    ligne(format!("\nTempérature après le débit : {}", temperature()));

    if let Some(s) = tr {
        ligne(format!("\n## Temps réel, F64, {s} s cadencées\n"));
        let t = temps_reel(s);
        ligne(format!(
            "- Blocs : {} ; sous-débits : **{}** ; pire retard : {:.0} µs\n- Pire calcul d'un bloc : {:.0} µs pour une période de {:.0} µs\n- Charge moyenne : {:.1} % d'un cœur\n- Température après : {}",
            t.blocs,
            t.sous_debits,
            t.pire_retard_us,
            t.pire_calcul_us,
            BLOC as f64 / FS * 1e6,
            t.charge * 100.0,
            temperature()
        ));
    }

    if let Some(chemin) = valeur("--sortie") {
        std::fs::write(&chemin, &r).expect("écriture du rapport");
        eprintln!("rapport écrit : {chemin}");
    }
}
