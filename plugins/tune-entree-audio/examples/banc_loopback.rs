//! Banc de la comparaison octet pour octet à travers un périphérique en
//! boucle (Loopback Audio) — #5051.
//!
//! ```text
//! banc_loopback emettre <sortie> <reference.wav> <secondes>
//!     fabrique un signal 24 bits stéréo connu à la fréquence COURANTE de la
//!     sortie nommée, l'écrit dans <reference.wav>, puis le joue sur CETTE
//!     sortie et nulle autre (cpal, par son nom ; `afplay` ne sait jouer que
//!     sur la sortie par défaut du système).
//! banc_loopback comparer <reference.wav> <capture.wav>
//!     cherche la référence dans ce que la zone a reçu (en-tête WAV de flux
//!     compris) et compare octet pour octet.
//! ```
//!
//! Pas de ffmpeg, pas de rééchantillonnage : le signal est émis à la
//! fréquence que la sortie a déjà.

use std::io::Write;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("emettre") if a.len() == 5 => emettre(&a[2], &a[3], a[4].parse().unwrap()),
        Some("comparer") if a.len() == 4 => comparer(&a[2], &a[3]),
        _ => {
            eprintln!("usage : emettre <sortie> <ref.wav> <s> | comparer <ref.wav> <capture.wav>");
            std::process::exit(2)
        }
    }
}

/// Un générateur pseudo-aléatoire déterministe (xorshift).
struct Hasard(u64);
impl Hasard {
    fn suivant(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Signal connu, 24 bits : un sinus à gauche, un bruit à droite, à -6 dBFS.
fn signal(frequence: u32, secondes: u32) -> Vec<[i32; 2]> {
    let mut h = Hasard(0x5051_5051_dead_beef);
    (0..frequence * secondes)
        .map(|n| {
            let t = n as f64 / frequence as f64;
            let g = (4_194_303.0 * (2.0 * std::f64::consts::PI * 997.0 * t).sin()) as i32;
            let d = (h.suivant() % 8_388_608) as i32 - 4_194_304;
            [g, d]
        })
        .collect()
}

fn entete(frequence: u32, octets: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&frequence.to_le_bytes());
    v.extend_from_slice(&(frequence * 6).to_le_bytes());
    v.extend_from_slice(&6u16.to_le_bytes());
    v.extend_from_slice(&24u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets.to_le_bytes());
    v
}

fn emettre(sortie: &str, reference: &str, secondes: u32) {
    let host = cpal::default_host();
    let d = host
        .output_devices()
        .unwrap()
        .find(|d| d.description().map(|x| x.name() == sortie).unwrap_or(false))
        .unwrap_or_else(|| panic!("aucune sortie « {sortie} »"));
    let c = d.default_output_config().unwrap();
    assert_eq!(
        c.sample_format(),
        cpal::SampleFormat::F32,
        "sortie non flottante"
    );
    assert_eq!(c.channels(), 2, "sortie non stéréo");
    let f = c.sample_rate();
    let s = signal(f, secondes);
    let mut o = entete(f, (s.len() * 6) as u32);
    for t in &s {
        for v in t {
            o.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }
    std::fs::File::create(reference)
        .unwrap()
        .write_all(&o)
        .unwrap();
    eprintln!(
        "sortie « {sortie} » : {f} Hz, {} trames 24 bits → {reference}",
        s.len()
    );
    let (fin_tx, fin_rx) = std::sync::mpsc::channel();
    let mut i = 0usize;
    // Une seconde de silence avant, pour laisser la capture s'installer.
    let avant = f as usize;
    let total = avant + s.len();
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: f,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = d
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for t in data.chunks_mut(2) {
                    let v = if i >= avant && i < total {
                        let [g, d] = s[i - avant];
                        [g as f32 / 8_388_608.0, d as f32 / 8_388_608.0]
                    } else {
                        [0.0, 0.0]
                    };
                    t[0] = v[0];
                    t[1] = v[1];
                    i += 1;
                    if i == total + f as usize {
                        let _ = fin_tx.send(());
                    }
                }
            },
            |e| eprintln!("erreur de sortie : {e}"),
            None,
        )
        .unwrap();
    stream.play().unwrap();
    fin_rx.recv().unwrap();
    drop(stream);
    eprintln!("émission terminée");
}

fn corps_wav(v: &[u8]) -> (&[u8], u32, u16, u16) {
    assert_eq!(&v[..4], b"RIFF");
    let frequence = u32::from_le_bytes(v[24..28].try_into().unwrap());
    let canaux = u16::from_le_bytes(v[22..24].try_into().unwrap());
    let bits = u16::from_le_bytes(v[34..36].try_into().unwrap());
    let data = v.windows(4).position(|w| w == b"data").unwrap();
    (&v[data + 8..], frequence, canaux, bits)
}

fn echantillons(o: &[u8]) -> Vec<i32> {
    o.chunks_exact(3)
        .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
        .collect()
}

/// Ce que le gain `g` (appliqué en `f32`, comme un volume CoreAudio) fait
/// d'un échantillon 24 bits, puis la conversion de la capture.
fn avec_gain(x: i32, g: f32) -> i32 {
    let y = (x as f32 / 8_388_608.0) * g;
    ((y as f64 * 8_388_608.0).round() as i64).clamp(-8_388_608, 8_388_607) as i32
}

fn comparer(reference: &str, capture: &str) {
    let r = std::fs::read(reference).unwrap();
    let c = std::fs::read(capture).unwrap();
    let (r, fr, _, _) = corps_wav(&r);
    let (c, fc, cc, bc) = corps_wav(&c);
    println!("référence : {} octets ({fr} Hz, 24 bits, 2 voies)", r.len());
    println!(
        "reçu par la zone : {} octets ({fc} Hz, {bc} bits, {cc} voies, en-tête de flux)",
        c.len()
    );
    assert_eq!((fr, cc, bc), (fc, 2, 24), "formats différents");
    let (r, c) = (echantillons(r), echantillons(c));
    // Alignement : la référence commence par une trame (0, non nul) ; le
    // reçu, par du silence numérique jusqu'à elle.
    let Some(premier) = c.iter().position(|&x| x != 0) else {
        println!("RIEN REÇU (silence numérique)");
        std::process::exit(1);
    };
    let debut = premier - premier % 2;
    let n = r.len().min(c.len() - debut);
    let (r, recu) = (&r[..n], &c[debut..debut + n]);
    println!(
        "alignement : trame {} du reçu ; {} trames ({:.2} s) comparables ; avant : {} trames, toutes nulles : {}",
        debut / 2,
        n / 2,
        n as f64 / 2.0 / fr as f64,
        debut / 2,
        c[..debut].iter().all(|&x| x == 0)
    );
    let identiques = r.iter().zip(recu).filter(|(a, b)| a == b).count();
    println!("identiques octet pour octet : {identiques} / {n} échantillons");
    if identiques == n {
        println!("IDENTIQUE OCTET POUR OCTET");
        return;
    }
    // Sinon : un gain constant (volume de la sortie virtuelle) ? On le
    // retrouve en f32, puis on compare À CE GAIN PRÈS.
    let (mut somme, mut k) = (0f64, 0u32);
    for (a, b) in r.iter().zip(recu).take(40_000) {
        if a.abs() > 1 << 20 {
            somme += *b as f64 / *a as f64;
            k += 1;
        }
    }
    let estime = (somme / k.max(1) as f64) as f32;
    let meilleur = (-64i32..=64)
        .map(|d| f32::from_bits((estime.to_bits() as i32 + d) as u32))
        .max_by_key(|g| {
            r.iter()
                .zip(recu)
                .take(8_000)
                .filter(|(a, b)| avec_gain(**a, *g) == **b)
                .count()
        })
        .unwrap();
    let mut ecarts = std::collections::BTreeMap::<i32, usize>::new();
    for (a, b) in r.iter().zip(recu) {
        *ecarts.entry(b - avec_gain(*a, meilleur)).or_default() += 1;
    }
    println!(
        "gain constant retrouvé : {meilleur} ({:.3} dB)",
        20.0 * (meilleur as f64).log10()
    );
    println!("écarts au gain près (LSB 24 bits → échantillons) : {ecarts:?}");
    let max = ecarts.keys().map(|e| e.abs()).max().unwrap_or(0);
    if max <= 1 {
        println!(
            "IDENTIQUE AU GAIN PRÈS (±1 LSB) : aligné, sans trou, sans doublon, sans rééchantillonnage"
        );
    } else {
        println!("DIFFÉRENT : écart max {max} LSB");
        std::process::exit(1);
    }
}
