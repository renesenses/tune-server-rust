//! Import et export compatibles REW (Room EQ Wizard, John Mulcahy) :
//! - mesure au format texte (« Export measurement as text ») : lignes
//!   `fréquence SPL phase` précédées de commentaires `*` ;
//! - réponse impulsionnelle en WAV flottant 32 bits mono (« Export impulse
//!   response as WAV »), relue par le convolueur de Tune (`read_wav_ir`) ;
//! - filtres au format « Filter Settings file » (export avec C2).
//!
//! Le lecteur WAV minimal ci-dessous évite une dépendance supplémentaire ;
//! la version serveur réutilisera `convolver::read_wav_ir`.

use crate::analysis::LogResponse;

/// Export d'une réponse (grille quelconque) au format texte REW.
pub fn export_measurement_txt(name: &str, freqs: &[f64], spl_db: &[f64], phase_deg: Option<&[f64]>) -> String {
    let mut s = format!("* Measurement data measured by Tune calibration (#7632)\n* Name: {name}\n* Freq(Hz) SPL(dB) Phase(degrees)\n");
    for (i, (f, m)) in freqs.iter().zip(spl_db).enumerate() {
        let ph = phase_deg.map(|p| p[i]).unwrap_or(0.0);
        s.push_str(&format!("{f:.6} {m:.3} {ph:.4}\n"));
    }
    s
}

/// Import d'un export texte REW (ou de tout fichier « fréquence dB [phase] »).
pub fn import_measurement_txt(text: &str) -> Option<LogResponse> {
    let mut freqs = Vec::new();
    let mut mags = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with(['*', '#', ';']) {
            continue;
        }
        let nums: Vec<f64> = l
            .split([' ', '\t', ','])
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        if nums.len() >= 2 {
            freqs.push(nums[0]);
            mags.push(nums[1]);
        }
    }
    if freqs.len() < 2 {
        return None;
    }
    Some(LogResponse { freqs, magnitude_db: mags })
}

/// Écrit un WAV RIFF flottant 32 bits, `channels` canaux entrelacés.
pub fn wav_float32_bytes(sample_rate: u32, channels: u16, interleaved: &[f32]) -> Vec<u8> {
    let data_len = (interleaved.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * channels as u32 * 4).to_le_bytes());
    out.extend_from_slice(&(channels * 4).to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for v in interleaved {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Lit un WAV PCM 16/24/32 bits ou flottant 32 bits ; renvoie (taux, canaux, échantillons entrelacés f64).
pub fn wav_read(bytes: &[u8]) -> Result<(u32, u16, Vec<f64>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("pas un fichier RIFF/WAVE".into());
    }
    let mut pos = 12;
    let (mut fmt_tag, mut channels, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_end = (pos + 8 + len).min(bytes.len());
        let body = &bytes[pos + 8..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err("bloc fmt tronqué".into());
                }
                fmt_tag = u16::from_le_bytes([body[0], body[1]]);
                channels = u16::from_le_bytes([body[2], body[3]]);
                rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                bits = u16::from_le_bytes([body[14], body[15]]);
                if fmt_tag == 0xFFFE && body.len() >= 26 {
                    fmt_tag = u16::from_le_bytes([body[24], body[25]]);
                }
            }
            b"data" => data = Some(body),
            _ => {}
        }
        pos = pos + 8 + len + (len & 1);
    }
    let data = data.ok_or("bloc data absent")?;
    let samples: Vec<f64> = match (fmt_tag, bits) {
        (1, 16) => data.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0).collect(),
        (1, 24) => data
            .chunks_exact(3)
            .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f64 / 8_388_608.0)
            .collect(),
        (1, 32) => data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes(c.try_into().unwrap()) as f64 / 2_147_483_648.0)
            .collect(),
        (3, 32) => data.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
        (3, 64) => data.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect(),
        _ => return Err(format!("format WAV non géré : tag {fmt_tag}, {bits} bits")),
    };
    Ok((rate, channels, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aller_retour_wav_float() {
        let x = [0.5f32, -0.25, 0.125];
        let b = wav_float32_bytes(48_000, 1, &x);
        let (sr, ch, s) = wav_read(&b).unwrap();
        assert_eq!((sr, ch), (48_000, 1));
        assert_eq!(s, vec![0.5, -0.25, 0.125]);
    }

    #[test]
    fn aller_retour_mesure_txt() {
        let txt = export_measurement_txt("essai", &[20.0, 1000.0], &[70.5, 71.25], None);
        let r = import_measurement_txt(&txt).unwrap();
        assert_eq!(r.freqs, vec![20.0, 1000.0]);
        assert!((r.magnitude_db[1] - 71.25).abs() < 1e-9);
    }
}
