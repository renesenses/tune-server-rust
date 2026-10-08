//! Banc mémoire de l'analyse ReplayGain / DR (plafond `MAX_ANALYSIS_EST_BYTES`).
//!
//! La passe écarte toute piste dont l'empreinte ESTIMÉE dépasse 1,2 Go : tout
//! 24/192 de plus de 4 min 20, tout DSD64 de plus de 4 min 40. Ce binaire dit
//! ce que l'analyse coûte VRAIMENT, sur de vrais fichiers, selon la durée.
//!
//! ```text
//! # fabriquer un fichier synthétique (signal modulé, pas du silence)
//! cargo run --release -p tune-core --example banc_memoire_analyse -- generer flac|wav|dsf64|dsf256 <minutes> <chemin>
//! # mesurer : pic de mémoire résidente (VmHWM) et durée de l'analyse
//! cargo run --release -p tune-core --example banc_memoire_analyse -- mesurer <chemin> [passe|dr]
//! ```
//!
//! `passe` : `mesurer_intensite_plage_et_empreinte`, ce qu'appelle la passe
//! nominale. `dr` : `mesurer_intensite_et_plage`, ce qu'appelle le rattrapage
//! de la plage dynamique. Le pic est lu dans `/proc/self/status` (Linux).

use std::io::{BufWriter, Write};
use std::time::Instant;

/// Le signal : un sinus dont la fréquence et l'amplitude dérivent lentement,
/// plus un bruit faible. Assez varié pour que le fenêtrage relatif BS.1770 et
/// l'histogramme DR aient de quoi trier.
struct Signal {
    rate: f64,
    n: u64,
    phase: f64,
    graine: u64,
}

impl Signal {
    fn new(rate: u32) -> Self {
        Self {
            rate: rate as f64,
            n: 0,
            phase: 0.0,
            graine: 0x9E37_79B9_7F4A_7C15,
        }
    }
    fn suivant(&mut self) -> f64 {
        let t = self.n as f64 / self.rate;
        self.n += 1;
        let f = 440.0 + 220.0 * (t * 0.05).sin();
        self.phase += 2.0 * std::f64::consts::PI * f / self.rate;
        if self.phase > 2.0 * std::f64::consts::PI {
            self.phase -= 2.0 * std::f64::consts::PI;
        }
        let a = 0.05 + 0.4 * (0.5 + 0.5 * (t * 2.0 * std::f64::consts::PI / 37.0).sin());
        self.graine = self
            .graine
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bruit = ((self.graine >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 0.01;
        a * self.phase.sin() + bruit
    }
}

fn pcm24(rate: u32, frames: u64, mut ecrire: impl FnMut(&[u8])) {
    let mut s = Signal::new(rate);
    let mut tampon = Vec::with_capacity(6 * 65_536);
    for i in 0..frames {
        let v = (s.suivant() * 8_388_607.0) as i32;
        let b = v.to_le_bytes();
        // Stéréo : même signal, droite légèrement atténuée.
        let d = ((v as f64) * 0.9) as i32;
        let bd = d.to_le_bytes();
        tampon.extend_from_slice(&b[..3]);
        tampon.extend_from_slice(&bd[..3]);
        if tampon.len() >= 6 * 65_536 || i + 1 == frames {
            ecrire(&tampon);
            tampon.clear();
        }
    }
}

fn generer_wav(minutes: f64, chemin: &str) {
    let rate = 192_000u32;
    let frames = (minutes * 60.0 * rate as f64) as u64;
    let data = frames * 6;
    assert!(data + 36 <= u32::MAX as u64, "WAV > 4 Gio");
    let mut f = BufWriter::new(std::fs::File::create(chemin).unwrap());
    f.write_all(b"RIFF").unwrap();
    f.write_all(&((36 + data) as u32).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * 6).to_le_bytes()).unwrap();
    f.write_all(&6u16.to_le_bytes()).unwrap();
    f.write_all(&24u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(data as u32).to_le_bytes()).unwrap();
    pcm24(rate, frames, |b| f.write_all(b).unwrap());
    f.flush().unwrap();
}

fn generer_flac(minutes: f64, chemin: &str) {
    let rate = 192_000u32;
    let frames = (minutes * 60.0 * rate as f64) as u64;
    let mut enc = tune_core::audio::encoder::AudioEncoder::new("flac", rate, 24, 2);
    enc.start_sync().unwrap();
    pcm24(rate, frames, |b| enc.write_sync(b).unwrap());
    let octets = enc.finish_sync().unwrap();
    std::fs::write(chemin, octets).unwrap();
}

/// DSF stéréo, modulateur sigma-delta d'ordre 2, blocs de 4096 octets par
/// canal, bits LSB en tête (format DSF).
fn generer_dsf(dsd_rate: u32, minutes: f64, chemin: &str) {
    const BLOC: usize = 4096;
    let canaux = 2usize;
    let total = (minutes * 60.0 * dsd_rate as f64) as u64; // échantillons par canal
    let octets_par_canal = total.div_ceil(8) as usize;
    let blocs = octets_par_canal.div_ceil(BLOC);
    let data = (blocs * BLOC * canaux) as u64;
    let mut f = BufWriter::new(std::fs::File::create(chemin).unwrap());
    let taille = 28 + 52 + 12 + data;
    f.write_all(b"DSD ").unwrap();
    f.write_all(&28u64.to_le_bytes()).unwrap();
    f.write_all(&taille.to_le_bytes()).unwrap();
    f.write_all(&0u64.to_le_bytes()).unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&52u64.to_le_bytes()).unwrap();
    f.write_all(&1u32.to_le_bytes()).unwrap(); // version
    f.write_all(&0u32.to_le_bytes()).unwrap(); // DSD brut
    f.write_all(&2u32.to_le_bytes()).unwrap(); // stéréo
    f.write_all(&(canaux as u32).to_le_bytes()).unwrap();
    f.write_all(&dsd_rate.to_le_bytes()).unwrap();
    f.write_all(&1u32.to_le_bytes()).unwrap();
    f.write_all(&total.to_le_bytes()).unwrap();
    f.write_all(&(BLOC as u32).to_le_bytes()).unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(12 + data).to_le_bytes()).unwrap();
    // Le signal suit une cadence PCM fictive de dsd_rate / 16, tenue 16 bits.
    let mut s = Signal::new(dsd_rate / 16);
    let mut courant = 0.0f64;
    let mut integ = [[0.0f64; 2]; 2];
    let mut n: u64 = 0;
    let mut bloc = vec![[0u8; BLOC]; canaux];
    for _ in 0..blocs {
        for octet in 0..BLOC {
            for bit in 0..8 {
                if n % 16 == 0 {
                    courant = s.suivant() * 0.5;
                }
                for (c, i) in integ.iter_mut().enumerate() {
                    let x = if c == 0 { courant } else { courant * 0.9 };
                    let y = if i[1] >= 0.0 { 1.0 } else { -1.0 };
                    i[0] += x - y;
                    i[1] += i[0] - y;
                    if n < total && y > 0.0 {
                        bloc[c][octet] |= 1 << bit;
                    }
                }
                n += 1;
            }
        }
        for b in &mut bloc {
            f.write_all(b).unwrap();
            b.fill(0);
        }
    }
    f.flush().unwrap();
}

fn pic_residente_kio() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("generer") => {
            let minutes: f64 = args[3].parse().unwrap();
            let chemin = &args[4];
            let t = Instant::now();
            match args[2].as_str() {
                "wav" => generer_wav(minutes, chemin),
                "flac" => generer_flac(minutes, chemin),
                "dsf64" => generer_dsf(2_822_400, minutes, chemin),
                "dsf256" => generer_dsf(11_289_600, minutes, chemin),
                autre => panic!("format inconnu : {autre}"),
            }
            println!("GENERE {chemin} en {:.1} s", t.elapsed().as_secs_f64());
        }
        Some("mesurer") => {
            let chemin = args[2].clone();
            let mode = args.get(3).cloned().unwrap_or_else(|| "passe".into());
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let avant = pic_residente_kio().unwrap_or(0);
            let t = Instant::now();
            let mesure = rt.block_on(async {
                if mode == "dr" {
                    tune_core::audio::analyzer::mesurer_intensite_et_plage(&chemin).await
                } else {
                    tune_core::audio::analyzer::mesurer_intensite_plage_et_empreinte(&chemin)
                        .await
                        .mesure
                }
            });
            let duree = t.elapsed().as_secs_f64();
            println!(
                "MESURE {chemin} mode={mode} duree_s={duree:.1} pic_avant_kio={avant} \
                 pic_kio={} resultat={mesure:?}",
                pic_residente_kio().unwrap_or(0)
            );
        }
        _ => eprintln!(
            "usage : generer <flac|wav|dsf64|dsf256> <minutes> <chemin> | mesurer <chemin> [passe|dr]"
        ),
    }
}
