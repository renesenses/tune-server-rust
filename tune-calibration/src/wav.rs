//! Écrivain WAV des réponses impulsionnelles de correction (C3, TESS T10b).
//!
//! Le fichier est lu par le convolueur de Tune
//! (`tune-core/src/audio/convolver.rs`, `read_wav_ir`) et déposé par
//! `POST /api/v1/room-correction/ir/upload/{zone}?per_rate=1`. Il suit ce
//! que ce lecteur attend, sans rien de plus :
//!
//! * `RIFF`/`WAVE`, bloc `fmt ` de 18 octets (format 3 = IEEE float, PAS le
//!   format étendu `0xFFFE`), `cbSize` = 0 ; bloc `fact` (nombre de trames,
//!   exigé par la norme pour un format non PCM) ; bloc `data` ;
//! * flottant 64 bits (aucune perte depuis le calcul `f64`) ou 32 bits.
//!   ⚠️ `read_wav_ir` relit aujourd'hui TOUT en `f32` (le 64 bits y est
//!   arrondi au chargement) : le fichier 64 bits ne garde sa précision que
//!   pour un lecteur `f64` (portage du convolueur en `f64`, T09b) ;
//! * un canal par enceinte, entrelacés ; mêmes bornes que le convolueur
//!   (`convolver/par_cadence.rs`) : 8 à 768 kHz, 2^20 trames, 32 canaux.
//!
//! La caisse est pure (pas de `tune-core`) : le lecteur du convolueur n'est
//! pas appelé ici ; les essais le reproduisent pas à pas.

/// Précision des échantillons écrits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatEchantillon {
    Flottant32,
    Flottant64,
}

impl FormatEchantillon {
    pub fn bits(self) -> u16 {
        match self {
            Self::Flottant32 => 32,
            Self::Flottant64 => 64,
        }
    }

    /// `"f32"` ou `"f64"`.
    pub fn depuis(texte: &str) -> Option<Self> {
        match texte {
            "f32" | "float32" => Some(Self::Flottant32),
            "f64" | "float64" => Some(Self::Flottant64),
            _ => None,
        }
    }
}

pub const TRAMES_MAX: usize = 1 << 20;
pub const CANAUX_MAX: usize = 32;

/// Le fichier WAV des `canaux` (même longueur chacun) à `cadence`.
pub fn ecrire_wav(
    cadence: u32,
    canaux: &[Vec<f64>],
    format: FormatEchantillon,
) -> Result<Vec<u8>, String> {
    if !(8_000..=768_000).contains(&cadence) {
        return Err(format!("cadence {cadence} Hz : entre 8 et 768 kHz"));
    }
    if canaux.is_empty() || canaux.len() > CANAUX_MAX {
        return Err(format!("{} canaux : entre 1 et {CANAUX_MAX}", canaux.len()));
    }
    let trames = canaux[0].len();
    if trames == 0 || trames > TRAMES_MAX {
        return Err(format!("{trames} trames : entre 1 et {TRAMES_MAX}"));
    }
    if canaux.iter().any(|c| c.len() != trames) {
        return Err("canaux de longueurs différentes".into());
    }
    if canaux.iter().flatten().any(|v| !v.is_finite()) {
        return Err("échantillon non fini".into());
    }
    let nc = canaux.len() as u16;
    let octets = format.bits() / 8;
    let bloc = nc * octets;
    let data_len = trames as u32 * bloc as u32;
    let mut out = Vec::with_capacity(58 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    // 4 (WAVE) + 8+18 (fmt) + 8+4 (fact) + 8 + data
    out.extend_from_slice(&(50 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&18u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&nc.to_le_bytes());
    out.extend_from_slice(&cadence.to_le_bytes());
    out.extend_from_slice(&(cadence * bloc as u32).to_le_bytes());
    out.extend_from_slice(&bloc.to_le_bytes());
    out.extend_from_slice(&format.bits().to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(b"fact");
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&(trames as u32).to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..trames {
        for c in canaux {
            match format {
                FormatEchantillon::Flottant32 => {
                    out.extend_from_slice(&(c[i] as f32).to_le_bytes())
                }
                FormatEchantillon::Flottant64 => out.extend_from_slice(&c[i].to_le_bytes()),
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_bornes_du_convolueur_sont_refusees() {
        let un = vec![vec![1.0]];
        assert!(ecrire_wav(7_999, &un, FormatEchantillon::Flottant64).is_err());
        assert!(ecrire_wav(768_001, &un, FormatEchantillon::Flottant64).is_err());
        assert!(ecrire_wav(48_000, &[], FormatEchantillon::Flottant64).is_err());
        assert!(ecrire_wav(48_000, &vec![vec![0.0]; 33], FormatEchantillon::Flottant64).is_err());
        assert!(ecrire_wav(
            48_000,
            &[vec![0.0; 2], vec![0.0; 3]],
            FormatEchantillon::Flottant32
        )
        .is_err());
        assert!(ecrire_wav(48_000, &[vec![f64::NAN]], FormatEchantillon::Flottant32).is_err());
        assert!(ecrire_wav(
            48_000,
            &[vec![0.0; TRAMES_MAX + 1]],
            FormatEchantillon::Flottant32
        )
        .is_err());
    }

    #[test]
    fn la_relecture_de_la_caisse_rend_les_memes_echantillons() {
        let g: Vec<f64> = (0..100).map(|i| (i as f64 * 0.37).sin() / 3.0).collect();
        let d: Vec<f64> = g.iter().map(|v| -v * 0.5).collect();
        let b = ecrire_wav(
            48_000,
            &[g.clone(), d.clone()],
            FormatEchantillon::Flottant64,
        )
        .unwrap();
        let (sr, nc, s) = crate::rew::wav_read(&b).unwrap();
        assert_eq!((sr, nc), (48_000, 2));
        for i in 0..100 {
            assert_eq!(s[2 * i], g[i]);
            assert_eq!(s[2 * i + 1], d[i]);
        }
        assert_eq!(
            u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize,
            b.len() - 8
        );
    }
}
