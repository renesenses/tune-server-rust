//! TESS T11b — banc du modulateur DSD.
//!
//! ```text
//! cargo run --release -p tune-plugin-audio-support --features mesure-dsd --example banc_modulateur_dsd -- \
//!     [--dsf fichier.dsf] [--secondes 10]
//! ```
//!
//! Rend un tableau (Markdown) : pour DSD64, DSD128 et DSD256, et pour chaque
//! signal de la matrice, le niveau de la raie, le bruit dans la bande
//! (20 Hz – 20 kHz), le bruit hors bande (20 – 100 kHz), la plus haute raie
//! parasite, le SINAD, l'entrée maximale du quantificateur et le nombre de
//! remises à zéro (instabilités). Puis l'étalon d'ordre 2 (même mesure), le
//! coût CPU (temps réel × N, un cœur) et, avec `--dsf`, un fichier DSF stéréo
//! de contrôle (1 kHz à −20 dBFS, DSD64) pour un essai sur un DAC réel.
//! Aucun son n'est joué.

use tune_plugin_audio_support::modulateur_dsd::mesure::{
    analyser, bits_du_canal, reference_ordre_2, sinus,
};
use tune_plugin_audio_support::modulateur_dsd::{CadenceDsd, ModulateurDsd};

const FS: u32 = 44_100;
const N_FFT: usize = 1 << 20;

fn moduler(m: &mut ModulateurDsd, pcm: &[f64]) -> Vec<u8> {
    let mut sortie = vec![0u8; m.octets_pour(pcm.len() / m.canaux())];
    m.traiter(pcm, &mut sortie).expect("modulation");
    sortie
}

fn signal(nom: &str, trames: usize) -> (Vec<f64>, Option<f64>) {
    match nom {
        "silence" => (vec![0.0; trames * 2], None),
        "1 kHz −60 dBFS" => (sinus(FS, 1_000.0, 1e-3, trames, 2), Some(1_000.0)),
        "1 kHz −20 dBFS" => (sinus(FS, 1_000.0, 0.1, trames, 2), Some(1_000.0)),
        "1 kHz 0 dBFS" => (sinus(FS, 1_000.0, 1.0, trames, 2), Some(1_000.0)),
        "20 kHz 0 dBFS" => (sinus(FS, 20_000.0, 1.0, trames, 2), Some(20_000.0)),
        "intersample +3 dB" => (
            (0..trames)
                .flat_map(|i| {
                    let v = if (i / 2) % 2 == 0 { 1.0 } else { -1.0 };
                    [v, v]
                })
                .collect(),
            Some(11_025.0),
        ),
        _ => unreachable!(),
    }
}

fn ecrire_dsf(chemin: &str, octets_msb: &[u8], canaux: usize, cadence: u32) -> std::io::Result<()> {
    // DSF : blocs de 4 096 octets par canal, bit de poids FAIBLE = plus ancien.
    const BLOC: usize = 4096;
    let par_canal = octets_msb.len() / canaux;
    let blocs = par_canal.div_ceil(BLOC);
    let mut donnees = vec![0u8; blocs * BLOC * canaux];
    for i in 0..par_canal {
        for c in 0..canaux {
            let o = octets_msb[i * canaux + c].reverse_bits();
            let (b, j) = (i / BLOC, i % BLOC);
            donnees[(b * canaux + c) * BLOC + j] = o;
        }
    }
    let echantillons = (par_canal * 8) as u64;
    let taille_data = 12 + donnees.len() as u64;
    let total = 28 + 52 + taille_data;
    let mut f = Vec::with_capacity(total as usize);
    f.extend_from_slice(b"DSD ");
    f.extend_from_slice(&28u64.to_le_bytes());
    f.extend_from_slice(&total.to_le_bytes());
    f.extend_from_slice(&0u64.to_le_bytes());
    f.extend_from_slice(b"fmt ");
    f.extend_from_slice(&52u64.to_le_bytes());
    f.extend_from_slice(&1u32.to_le_bytes()); // version
    f.extend_from_slice(&0u32.to_le_bytes()); // DSD brut
    f.extend_from_slice(&(if canaux == 2 { 2u32 } else { 1 }).to_le_bytes());
    f.extend_from_slice(&(canaux as u32).to_le_bytes());
    f.extend_from_slice(&cadence.to_le_bytes());
    f.extend_from_slice(&1u32.to_le_bytes()); // bits par échantillon
    f.extend_from_slice(&echantillons.to_le_bytes());
    f.extend_from_slice(&(BLOC as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&taille_data.to_le_bytes());
    f.extend_from_slice(&donnees);
    std::fs::write(chemin, f)
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
        .unwrap_or(10.0);

    println!("# Banc du modulateur DSD (TESS T11b)\n");
    println!(
        "Ordre {}, HINF {}, dither ±{}, 0 dBFS = {} % de modulation, limiteur {} %.\n",
        tune_plugin_audio_support::modulateur_dsd::ORDRE,
        tune_plugin_audio_support::modulateur_dsd::HINF,
        tune_plugin_audio_support::modulateur_dsd::DITHER,
        tune_plugin_audio_support::modulateur_dsd::NIVEAU_REFERENCE * 100.0,
        tune_plugin_audio_support::modulateur_dsd::LIMITE_MODULATION * 100.0,
    );
    println!(
        "dB relatifs à 100 % de modulation (0 dB SACD = −6,02 dB). FFT de {N_FFT} points, fenêtre de Blackman-Harris à 7 termes.\n"
    );
    println!(
        "| Cadence | Signal | Raie | Bruit 20 Hz–20 kHz | Bruit 20–100 kHz | Raie parasite max | SINAD | Entrée max | Remises à zéro |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let noms = [
        "silence",
        "1 kHz −60 dBFS",
        "1 kHz −20 dBFS",
        "1 kHz 0 dBFS",
        "20 kHz 0 dBFS",
        "intersample +3 dB",
    ];
    for cadence in [CadenceDsd::Dsd64, CadenceDsd::Dsd128, CadenceDsd::Dsd256] {
        for nom in noms {
            let mut m = ModulateurDsd::new(FS, 2, cadence).expect("modulateur");
            let trames = (N_FFT + m.retard_dsd() as usize + 4096) / m.rapport() + 1;
            let (pcm, f0) = signal(nom, trames);
            let octets = moduler(&mut m, &pcm);
            let bits = bits_du_canal(&octets, 2, 0);
            let a = analyser(&bits[bits.len() - N_FFT..], f64::from(m.cadence_dsd()), f0);
            let raie = f0.map_or("—".to_string(), |_| format!("{:.2}", a.signal_db));
            let sinad = f0.map_or("—".to_string(), |_| format!("{:.1}", a.sinad_db));
            println!(
                "| {cadence:?} | {nom} | {raie} | {:.1} | {:.1} | {:.1} | {sinad} | {:.2} | {} |",
                a.bruit_bande_db,
                a.bruit_hors_bande_db,
                a.raie_max_db,
                m.entree_max(),
                m.reinitialisations()
            );
        }
    }

    println!("\n## Étalon : modulateur de manuel d'ordre 2, DSD64, 1 kHz à 50 %\n");
    let fs_dsd = 2_822_400.0;
    let entree: Vec<f64> = (0..N_FFT)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / fs_dsd).sin())
        .collect();
    let r = analyser(&reference_ordre_2(&entree), fs_dsd, Some(1_000.0));
    println!(
        "Raie {:.2} dB, bruit 20 Hz–20 kHz {:.1} dB, hors bande {:.1} dB, SINAD {:.1} dB.",
        r.signal_db, r.bruit_bande_db, r.bruit_hors_bande_db, r.sinad_db
    );

    println!("\n## Coût CPU (un cœur, {secondes} s de stéréo 44,1 kHz, 1 kHz à −20 dBFS)\n");
    println!("| Cadence | Temps | Temps réel × |");
    println!("|---|---|---|");
    let trames = (secondes * f64::from(FS)) as usize;
    let pcm = sinus(FS, 1_000.0, 0.1, trames, 2);
    for cadence in [CadenceDsd::Dsd64, CadenceDsd::Dsd128, CadenceDsd::Dsd256] {
        let mut m = ModulateurDsd::new(FS, 2, cadence).expect("modulateur");
        let mut sortie = vec![0u8; m.octets_pour(4096)];
        let debut = std::time::Instant::now();
        for bloc in pcm.chunks(4096 * 2) {
            m.traiter(bloc, &mut sortie).expect("modulation");
        }
        let t = debut.elapsed().as_secs_f64();
        println!("| {cadence:?} | {t:.3} s | {:.1} |", secondes / t);
    }

    if let Some(chemin) = valeur("--dsf") {
        let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).expect("modulateur");
        let octets = moduler(&mut m, &pcm);
        ecrire_dsf(&chemin, &octets, 2, m.cadence_dsd()).expect("écriture DSF");
        println!("\nFichier de contrôle : {chemin} (DSD64, 1 kHz à −20 dBFS, {secondes} s).");
    }
}
