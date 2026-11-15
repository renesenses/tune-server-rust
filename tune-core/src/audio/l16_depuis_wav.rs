//! #8722 — vue audio/L16 d'un WAV PCM16 complet, sans copie du fichier.
//! RFC 2586 §3 : échantillons signés 16 bits en ordre réseau, entrelacés.
//! https://www.rfc-editor.org/rfc/rfc2586
//! L'en-tête RIFF est retiré ; les plages, même impaires, restent exactes.
use crate::http::streamer::SourceVirtuelle;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug)]
pub(crate) struct L16DepuisWav {
    fichier: Mutex<File>,
    debut_pcm: u64,
    octets_pcm: u64,
    pub cadence: u32,
    pub canaux: u16,
}

impl L16DepuisWav {
    pub fn ouvrir(path: &Path) -> Result<Self, String> {
        let mut fichier = File::open(path).map_err(|e| e.to_string())?;
        let taille = fichier.metadata().map_err(|e| e.to_string())?.len();
        let mut header = [0u8; 12];
        fichier.read_exact(&mut header).map_err(|e| e.to_string())?;
        if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
            return Err("L16 : fichier WAV RIFF attendu".into());
        }
        let mut format = None;
        let mut position = 12u64;
        while position.checked_add(8).is_some_and(|n| n <= taille) {
            fichier
                .seek(SeekFrom::Start(position))
                .map_err(|e| e.to_string())?;
            let mut chunk = [0u8; 8];
            fichier.read_exact(&mut chunk).map_err(|e| e.to_string())?;
            let n = u64::from(u32::from_le_bytes(chunk[4..].try_into().unwrap()));
            let debut = position + 8;
            if n > taille - debut {
                return Err("L16 : bloc WAV tronqué".into());
            }
            if &chunk[..4] == b"fmt " {
                if n < 16 {
                    return Err("L16 : format WAV tronqué".into());
                }
                let mut fmt = [0u8; 16];
                fichier.read_exact(&mut fmt).map_err(|e| e.to_string())?;
                let codec = u16::from_le_bytes(fmt[..2].try_into().unwrap());
                let canaux = u16::from_le_bytes(fmt[2..4].try_into().unwrap());
                let cadence = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
                let trame = u16::from_le_bytes(fmt[12..14].try_into().unwrap());
                let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap());
                if codec != 1
                    || bits != 16
                    || canaux == 0
                    || cadence == 0
                    || canaux.checked_mul(2) != Some(trame)
                {
                    return Err("L16 : PCM signé 16 bits requis après le traitement".into());
                }
                format = Some((cadence, canaux, trame));
            } else if &chunk[..4] == b"data" {
                let (cadence, canaux, trame) =
                    format.ok_or("L16 : format absent avant les données")?;
                if n % u64::from(trame) != 0 {
                    return Err("L16 : trame PCM incomplète".into());
                }
                return Ok(Self {
                    fichier: Mutex::new(fichier),
                    debut_pcm: debut,
                    octets_pcm: n,
                    cadence,
                    canaux,
                });
            }
            position = debut
                .checked_add(n)
                .and_then(|x| x.checked_add(n % 2))
                .ok_or("L16 : taille WAV hors limites")?;
        }
        Err("L16 : données PCM absentes".into())
    }

    pub fn ouvrir_pour_mime(path: &Path, mime: &str) -> Result<Self, String> {
        let source = Self::ouvrir(path)?;
        if source.mime() != mime {
            return Err(format!(
                "L16 : le WAV produit ({}) ne correspond pas au format annoncé ({mime})",
                source.mime()
            ));
        }
        Ok(source)
    }

    pub fn mime(&self) -> String {
        format!("audio/L16;rate={};channels={}", self.cadence, self.canaux)
    }
}

impl SourceVirtuelle for L16DepuisWav {
    fn total(&self) -> u64 {
        self.octets_pcm
    }

    fn lire(
        &self,
        debut: u64,
        longueur: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut reste = longueur.min(self.octets_pcm.saturating_sub(debut));
        if reste == 0 {
            return Ok(());
        }
        let mut fichier = self
            .fichier
            .lock()
            .map_err(|_| "L16 : lecture interrompue")?;
        let mut ignorer = (debut % 2) as usize;
        fichier
            .seek(SeekFrom::Start(self.debut_pcm + (debut & !1)))
            .map_err(|e| e.to_string())?;
        while reste != 0 {
            let besoin = reste.saturating_add(ignorer as u64).min(64 * 1024) as usize;
            let n = (besoin + 1) & !1;
            let mut bytes = vec![0u8; n];
            fichier.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            for sample in bytes.chunks_exact_mut(2) {
                sample.swap(0, 1);
            }
            let rendu = (bytes.len() - ignorer).min(reste as usize);
            rendre(bytes[ignorer..ignorer + rendu].to_vec())?;
            reste -= rendu as u64;
            ignorer = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fichier(samples: &[i16], bits: u16) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pcm.wav");
        let mut bytes = crate::audio::wav::build_wav_header_with_data_size(
            1,
            44_100,
            bits,
            (samples.len() * 2) as u32,
        )
        .to_vec();
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn l16_8722_ordre_reseau_sans_entete_et_plages_impaires() {
        let (_dir, path) = fichier(&[0x1234, -2, 0x5678], 16);
        let source = L16DepuisWav::ouvrir(&path).unwrap();
        assert_eq!(source.mime(), "audio/L16;rate=44100;channels=1");
        assert_eq!(source.total(), 6);
        for (debut, longueur, attendu) in [
            (0, 6, vec![0x12, 0x34, 0xff, 0xfe, 0x56, 0x78]),
            (1, 3, vec![0x34, 0xff, 0xfe]),
            (3, u64::MAX, vec![0xfe, 0x56, 0x78]),
            (6, 1, vec![]),
        ] {
            let mut bytes = Vec::new();
            source
                .lire(debut, longueur, &mut |chunk| {
                    bytes.extend(chunk);
                    Ok(())
                })
                .unwrap();
            assert_eq!(bytes, attendu, "plage {debut}/{longueur}");
        }
    }

    #[test]
    fn l16_8722_plusieurs_blocs_et_refus_du_24_bits() {
        let samples: Vec<i16> = (0..70_000).map(|n| (n % 32_000) as i16).collect();
        let (_dir, path) = fichier(&samples, 16);
        let source = L16DepuisWav::ouvrir(&path).unwrap();
        let mut bytes = Vec::new();
        source
            .lire(1, source.total() - 2, &mut |chunk| {
                bytes.extend(chunk);
                Ok(())
            })
            .unwrap();
        let attendu: Vec<u8> = samples.iter().flat_map(|n| n.to_be_bytes()).collect();
        assert_eq!(bytes, attendu[1..attendu.len() - 1]);
        let (_dir24, path24) = fichier(&[1, 2, 3], 24);
        assert!(
            L16DepuisWav::ouvrir(&path24).is_err(),
            "ne pas réétiqueter du 24 bits"
        );
    }
}
