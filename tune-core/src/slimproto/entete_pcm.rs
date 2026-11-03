//! #6475 — ce que Squeezelite doit recevoir pour jouer un WAV ou un AIFF.
//!
//! Squeezelite ne lit PAS l'en-tête d'un flux HTTP : `pcm.c` n'appelle
//! `_check_header()` que pour un fichier local (`stream.state ==
//! STREAMING_FILE`) ou avec l'option `-W` (`pcm_check_header`). Sans elle,
//! il prend les quatre champs PCM du `strm s` pour argent comptant
//! (`pcm_open()` : `sample_size = size - '0' + 1`, `sample_rate =
//! sample_rates[rate - '0']`, `channels = chan - '0'`, `bigendian =
//! (endianness == '0')`) et joue TOUT ce qu'il reçoit comme des
//! échantillons, en-tête compris. Un en-tête de 44 octets devant du 24 bits
//! stéréo (trames de 6 octets) décale toute la piste : du bruit.
//!
//! D'où ce module, qui lit l'en-tête du fichier servi :
//!
//! - les vrais paramètres, à coder dans les tables vérifiées (tailles `'0'` à
//!   `'3'` de `pcm_sample_sizes`, LMS `Slim/Player/Squeezebox.pm` ; fréquences
//!   `'0'` à `'>'` de `sample_rates[]`, `pcm.c` l. 65, et de
//!   `pcm_sample_rates`, LMS `Slim/Player/SqueezePlay.pm`) ;
//! - la TRANCHE d'octets des échantillons, que le `strm s` demande par
//!   `Range: bytes=<début>-<fin>`. C'est ce que fait LMS lui-même
//!   (`Slim/Player/Protocols/HTTP.pm` : « Always add Range to exclude trailing
//!   metadata or garbage (aif/mp4...) », `Range: bytes=audio_offset-…`), et
//!   Squeezelite envoie la requête du `strm s` telle quelle (`stream.c`,
//!   `stream_sock` puis `send_header`).
//!
//! Tout ce qui n'est pas certain est REFUSÉ : jamais de `'?'` dans un champ
//! PCM (`'?' - '0'` vaut 15, soit `sample_rates[15] = 0`), jamais d'en-tête
//! joué comme du son.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Fréquences PCM de SlimProto, dans l'ordre de leur code (`'0'` + rang) :
/// `sample_rates[]` de `pcm.c` (Squeezelite) et `pcm_sample_rates` de LMS
/// (`SqueezePlay.pm`), jusqu'à `'>'` = 384 kHz. Le rang 15 (`'?'`) vaut 0 dans
/// Squeezelite et n'est jamais utilisé.
pub(crate) const FREQUENCES_PCM: [u32; 15] = [
    11_025, 22_050, 32_000, 44_100, 48_000, 8_000, 12_000, 16_000, 24_000, 96_000, 88_200, 176_400,
    192_000, 352_800, 384_000,
];

/// Les quatre champs PCM d'un `strm s`, tous connus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChampsPcm {
    pub taille: u8,
    pub frequence: u8,
    pub canaux: u8,
    pub boutisme: u8,
}

/// Code les paramètres PCM pour Squeezelite, ou dit pourquoi on ne peut pas.
///
/// - taille : 16, 24 ou 32 bits entiers (`'1'`..`'3'`) ; 8 bits seulement en
///   gros-boutiste (AIFF, signé) — le 8 bits WAV est NON signé et
///   Squeezelite le décale comme un signé (`*iptr << 24`) ;
/// - canaux : 1 ou 2, les seuls que `pcm.c` sait sortir ;
/// - fréquence : présente dans [`FREQUENCES_PCM`].
pub fn coder_pcm(
    frequence: u32,
    bits: u32,
    canaux: u32,
    gros_boutiste: bool,
) -> Result<ChampsPcm, String> {
    let taille = match bits {
        8 if gros_boutiste => b'0',
        16 => b'1',
        24 => b'2',
        32 => b'3',
        autre => return Err(format!("profondeur {autre} bits non jouable en PCM")),
    };
    let rang = FREQUENCES_PCM
        .iter()
        .position(|&f| f == frequence)
        .ok_or_else(|| format!("fréquence {frequence} Hz hors de la table SlimProto"))?;
    if !(1..=2).contains(&canaux) {
        return Err(format!("{canaux} canaux : Squeezelite ne sort que 1 ou 2"));
    }
    Ok(ChampsPcm {
        taille,
        frequence: b'0' + rang as u8,
        canaux: b'0' + canaux as u8,
        boutisme: if gros_boutiste { b'0' } else { b'1' },
    })
}

/// Ce que l'en-tête d'un WAV ou d'un AIFF dit des échantillons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntetePcm {
    pub frequence: u32,
    pub bits: u32,
    pub canaux: u32,
    pub gros_boutiste: bool,
    /// Premier octet des échantillons dans le fichier.
    pub debut: u64,
    /// Nombre d'octets d'échantillons, borné à la fin réelle du fichier et
    /// ramené à un nombre entier de trames.
    pub longueur: u64,
}

impl EntetePcm {
    /// Les champs du `strm s`, ou le refus.
    pub fn champs(&self) -> Result<ChampsPcm, String> {
        coder_pcm(self.frequence, self.bits, self.canaux, self.gros_boutiste)
    }

    /// Dernier octet (inclus) de la tranche, pour `Range: bytes=debut-fin`.
    pub fn fin(&self) -> u64 {
        self.debut + self.longueur - 1
    }
}

fn u16le(b: &[u8]) -> u32 {
    u32::from(u16::from_le_bytes([b[0], b[1]]))
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Termine une analyse : borne la tranche au fichier, l'aligne sur la trame,
/// refuse une tranche vide.
fn borner(mut e: EntetePcm, taille_fichier: u64) -> Result<EntetePcm, String> {
    if e.debut >= taille_fichier {
        return Err("en-tête tronqué : aucun échantillon après l'en-tête".into());
    }
    e.longueur = e.longueur.min(taille_fichier - e.debut);
    let trame = u64::from(e.canaux) * u64::from(e.bits / 8);
    if trame == 0 {
        return Err("trame PCM de taille nulle".into());
    }
    e.longueur -= e.longueur % trame;
    if e.longueur == 0 {
        return Err("aucune trame entière après l'en-tête".into());
    }
    Ok(e)
}

/// Lit l'en-tête RIFF/WAVE : chunk `fmt ` (PCM, ou WAVE_FORMAT_EXTENSIBLE de
/// sous-format PCM), puis le chunk `data` et son offset, quels que soient les
/// chunks rencontrés avant (`LIST`, `id3 `, `bext`, `JUNK`…).
pub fn lire_wav(chemin: &Path) -> Result<EntetePcm, String> {
    let mut f = File::open(chemin).map_err(|e| format!("ouverture : {e}"))?;
    let taille = f.metadata().map_err(|e| e.to_string())?.len();
    let mut tete = [0u8; 12];
    f.read_exact(&mut tete)
        .map_err(|_| "en-tête RIFF tronqué".to_string())?;
    if &tete[0..4] != b"RIFF" || &tete[8..12] != b"WAVE" {
        return Err("pas un fichier RIFF/WAVE".into());
    }
    let mut fmt: Option<(u32, u32, u32)> = None; // fréquence, bits, canaux
    let mut pos = 12u64;
    loop {
        if pos + 8 > taille {
            return Err("chunk data introuvable (en-tête tronqué)".into());
        }
        f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        let mut entete = [0u8; 8];
        f.read_exact(&mut entete)
            .map_err(|_| "en-tête de chunk tronqué".to_string())?;
        let longueur = u64::from(u32le(&entete[4..8]));
        let corps = pos + 8;
        match &entete[0..4] {
            b"fmt " => {
                if longueur < 16 || corps + 16 > taille {
                    return Err("chunk fmt tronqué".into());
                }
                let mut b = vec![0u8; longueur.min(40) as usize];
                f.read_exact(&mut b)
                    .map_err(|_| "chunk fmt tronqué".to_string())?;
                let format = u16le(&b[0..2]);
                let canaux = u16le(&b[2..4]);
                let frequence = u32le(&b[4..8]);
                let alignement = u16le(&b[12..14]);
                let bits = u16le(&b[14..16]);
                let pcm = match format {
                    1 => true,
                    0xFFFE => {
                        // WAVE_FORMAT_EXTENSIBLE : cbSize(2) validBits(2)
                        // channelMask(4) puis le GUID du sous-format, dont les
                        // deux premiers octets portent le code (1 = PCM).
                        if b.len() < 26 {
                            return Err("chunk fmt extensible tronqué".into());
                        }
                        u16le(&b[24..26]) == 1
                    }
                    _ => false,
                };
                if !pcm {
                    return Err(format!("WAV non PCM entier (format {format:#06x})"));
                }
                if !bits.is_multiple_of(8) || alignement != canaux * (bits / 8) {
                    return Err(format!(
                        "WAV incohérent : {bits} bits, {canaux} canaux, trame de {alignement} octets"
                    ));
                }
                fmt = Some((frequence, bits, canaux));
            }
            b"data" => {
                let (frequence, bits, canaux) = fmt.ok_or("chunk data avant le chunk fmt")?;
                return borner(
                    EntetePcm {
                        frequence,
                        bits,
                        canaux,
                        gros_boutiste: false,
                        debut: corps,
                        longueur,
                    },
                    taille,
                );
            }
            _ => {}
        }
        // Les chunks RIFF sont alignés sur deux octets.
        pos = corps + longueur + (longueur % 2);
    }
}

/// Lit l'en-tête FORM/AIFF ou AIFC par [`crate::audio::aiff::parse_aiff`].
/// AIFC : seulement `NONE` (gros-boutiste) et `sowt` (petit-boutiste).
pub fn lire_aiff(chemin: &Path) -> Result<EntetePcm, String> {
    let taille = std::fs::metadata(chemin).map_err(|e| e.to_string())?.len();
    let chemin_str = chemin.to_str().ok_or("chemin non UTF-8")?;
    let info = crate::audio::aiff::parse_aiff(chemin_str)?;
    let gros_boutiste = match info.compression.as_deref() {
        None => true,
        Some(c) if c.eq_ignore_ascii_case("NONE") => true,
        Some("sowt") => false,
        Some(autre) => return Err(format!("AIFC compressé ({autre})")),
    };
    let frequence = info.sample_rate.round();
    if (frequence - info.sample_rate).abs() > 0.01 || frequence <= 0.0 {
        return Err(format!(
            "fréquence AIFF {} Hz non entière",
            info.sample_rate
        ));
    }
    let bits = u32::from(info.bits_per_sample);
    if !bits.is_multiple_of(8) || bits == 0 {
        return Err(format!("AIFF de {bits} bits non jouable"));
    }
    let canaux = u32::from(info.channels);
    let attendus = u64::from(info.num_frames) * u64::from(canaux) * u64::from(bits / 8);
    borner(
        EntetePcm {
            frequence: frequence as u32,
            bits,
            canaux,
            gros_boutiste,
            debut: info.data_offset,
            longueur: info.data_size.min(attendus),
        },
        taille,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Échantillons reconnaissables : l'octet `i` vaut `0xA0 + i % 16`. Le
    /// premier octet servi doit être `0xA0`, pas un octet d'en-tête.
    fn echantillons(n: usize) -> Vec<u8> {
        (0..n).map(|i| 0xA0 + (i % 16) as u8).collect()
    }

    fn chunk(id: &[u8; 4], corps: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(corps.len() as u32).to_le_bytes());
        v.extend_from_slice(corps);
        if corps.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn fmt_pcm(format: u16, canaux: u16, frequence: u32, bits: u16) -> Vec<u8> {
        let align = canaux * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(&format.to_le_bytes());
        b.extend_from_slice(&canaux.to_le_bytes());
        b.extend_from_slice(&frequence.to_le_bytes());
        b.extend_from_slice(&(frequence * u32::from(align)).to_le_bytes());
        b.extend_from_slice(&align.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b
    }

    fn fmt_extensible(canaux: u16, frequence: u32, bits: u16, sous_format: u16) -> Vec<u8> {
        let mut b = fmt_pcm(0xFFFE, canaux, frequence, bits);
        b.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        b.extend_from_slice(&bits.to_le_bytes()); // validBits
        b.extend_from_slice(&3u32.to_le_bytes()); // channelMask
        b.extend_from_slice(&sous_format.to_le_bytes());
        b.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
        ]);
        b
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let corps: Vec<u8> = chunks.concat();
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(4 + corps.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(&corps);
        v
    }

    fn ecrire(octets: &[u8], suffixe: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(suffixe).tempfile().unwrap();
        f.write_all(octets).unwrap();
        f.flush().unwrap();
        f
    }

    /// Vérifie la tranche OCTET POUR OCTET : elle commence au premier
    /// échantillon et contient exactement les échantillons.
    fn verifier_tranche(fichier: &[u8], e: &EntetePcm, attendus: &[u8]) {
        let tranche = &fichier[e.debut as usize..=e.fin() as usize];
        assert_eq!(
            tranche, attendus,
            "la tranche servie doit être les échantillons, et eux seuls"
        );
    }

    #[test]
    fn wav_16_24_32_bits_de_44_1_a_384_khz() {
        for (bits, frequence, code_taille, code_freq) in [
            (16u16, 44_100u32, b'1', b'3'),
            (24, 48_000, b'2', b'4'),
            (24, 88_200, b'2', b':'),
            (24, 96_000, b'2', b'9'),
            (24, 176_400, b'2', b';'),
            (24, 192_000, b'2', b'<'),
            (32, 352_800, b'3', b'='),
            (32, 384_000, b'3', b'>'),
        ] {
            let trame = 2 * usize::from(bits / 8);
            let donnees = echantillons(trame * 10);
            let octets = riff(&[
                chunk(b"fmt ", &fmt_pcm(1, 2, frequence, bits)),
                chunk(b"data", &donnees),
            ]);
            let f = ecrire(&octets, ".wav");
            let e = lire_wav(f.path()).unwrap();
            assert_eq!(
                (e.frequence, e.bits, e.canaux, e.gros_boutiste),
                (frequence, u32::from(bits), 2, false)
            );
            assert_eq!(e.debut, 44, "WAV canonique : échantillons à l'octet 44");
            verifier_tranche(&octets, &e, &donnees);
            assert_eq!(
                e.champs().unwrap(),
                ChampsPcm {
                    taille: code_taille,
                    frequence: code_freq,
                    canaux: b'2',
                    boutisme: b'1'
                },
                "{bits} bits / {frequence} Hz"
            );
        }
    }

    #[test]
    fn wav_extensible_pcm_accepte_et_flottant_refuse() {
        let donnees = echantillons(6 * 8);
        let octets = riff(&[
            chunk(b"fmt ", &fmt_extensible(2, 96_000, 24, 1)),
            chunk(b"data", &donnees),
        ]);
        let f = ecrire(&octets, ".wav");
        let e = lire_wav(f.path()).unwrap();
        assert_eq!(e.debut, 12 + 8 + 40 + 8, "fmt extensible de 40 octets");
        verifier_tranche(&octets, &e, &donnees);
        assert_eq!(e.champs().unwrap().taille, b'2');

        let flottant = riff(&[
            chunk(b"fmt ", &fmt_extensible(2, 96_000, 32, 3)),
            chunk(b"data", &echantillons(64)),
        ]);
        let f = ecrire(&flottant, ".wav");
        assert!(lire_wav(f.path()).is_err(), "WAV flottant : refusé");
        let f = ecrire(
            &riff(&[
                chunk(b"fmt ", &fmt_pcm(3, 2, 44_100, 32)),
                chunk(b"data", &echantillons(64)),
            ]),
            ".wav",
        );
        assert!(
            lire_wav(f.path()).is_err(),
            "WAVE_FORMAT_IEEE_FLOAT : refusé"
        );
    }

    /// Des chunks avant `data` (dont un de taille impaire, avec son octet de
    /// bourrage) et après : la tranche ne prend que les échantillons.
    #[test]
    fn chunks_inattendus_avant_et_apres_data() {
        let donnees = echantillons(4 * 25);
        let octets = riff(&[
            chunk(b"JUNK", &[0u8; 28]),
            chunk(b"fmt ", &fmt_pcm(1, 2, 44_100, 16)),
            chunk(b"LIST", b"INFOISFT\x05\x00\x00\x00Lavf\x00"),
            chunk(b"id3 ", &[0x49; 7]),
            chunk(b"data", &donnees),
            chunk(b"LIST", b"INFOIART\x04\x00\x00\x00abc\x00"),
        ]);
        let f = ecrire(&octets, ".wav");
        let e = lire_wav(f.path()).unwrap();
        assert!(e.debut > 44, "l'en-tête est plus long que 44 octets");
        assert_eq!(octets[e.debut as usize], 0xA0);
        verifier_tranche(&octets, &e, &donnees);
    }

    #[test]
    fn en_tete_tronque_ou_incoherent_refuse() {
        // Coupé au milieu du chunk fmt.
        let complet = riff(&[
            chunk(b"fmt ", &fmt_pcm(1, 2, 44_100, 16)),
            chunk(b"data", &echantillons(64)),
        ]);
        let f = ecrire(&complet[..30], ".wav");
        assert!(lire_wav(f.path()).is_err());
        // Pas de chunk data.
        let f = ecrire(&riff(&[chunk(b"fmt ", &fmt_pcm(1, 2, 44_100, 16))]), ".wav");
        assert!(lire_wav(f.path()).is_err());
        // data avant fmt.
        let f = ecrire(
            &riff(&[
                chunk(b"data", &echantillons(64)),
                chunk(b"fmt ", &fmt_pcm(1, 2, 44_100, 16)),
            ]),
            ".wav",
        );
        assert!(lire_wav(f.path()).is_err());
        // Pas un RIFF.
        let f = ecrire(b"fLaC\x00\x00\x00\x22", ".wav");
        assert!(lire_wav(f.path()).is_err());
        // data annonce plus que le fichier : borné, aligné sur la trame.
        let mut court = riff(&[
            chunk(b"fmt ", &fmt_pcm(1, 2, 44_100, 24)),
            chunk(b"data", &echantillons(60)),
        ]);
        court.truncate(44 + 33);
        let f = ecrire(&court, ".wav");
        let e = lire_wav(f.path()).unwrap();
        assert_eq!(e.longueur, 30, "33 octets restants → 5 trames de 6");
    }

    #[test]
    fn parametres_hors_table_refuses_jamais_de_point_d_interrogation() {
        assert!(coder_pcm(705_600, 24, 2, false).is_err(), "au-delà de '>'");
        assert!(
            coder_pcm(22_000, 16, 2, false).is_err(),
            "fréquence hors table"
        );
        assert!(coder_pcm(44_100, 20, 2, false).is_err());
        assert!(
            coder_pcm(44_100, 8, 2, false).is_err(),
            "8 bits WAV non signé"
        );
        assert!(coder_pcm(44_100, 16, 6, false).is_err(), "5.1");
        assert_eq!(coder_pcm(44_100, 8, 1, true).unwrap().taille, b'0');
        for f in FREQUENCES_PCM {
            for c in [
                coder_pcm(f, 24, 2, false).unwrap(),
                coder_pcm(f, 16, 1, true).unwrap(),
            ] {
                assert_ne!(c.frequence, b'?');
                assert!(![c.taille, c.canaux, c.boutisme].contains(&b'?'));
            }
        }
        let f = ecrire(
            &riff(&[
                chunk(b"fmt ", &fmt_pcm(1, 6, 48_000, 24)),
                chunk(b"data", &echantillons(36)),
            ]),
            ".wav",
        );
        assert!(
            lire_wav(f.path()).unwrap().champs().is_err(),
            "WAV 5.1 refusé"
        );
    }

    fn aiff(
        canaux: u16,
        bits: u16,
        frequence_ext: [u8; 10],
        donnees: &[u8],
        aifc: Option<&[u8; 4]>,
    ) -> Vec<u8> {
        let trame = u32::from(canaux) * u32::from(bits / 8);
        let mut comm = Vec::new();
        comm.extend_from_slice(&canaux.to_be_bytes());
        comm.extend_from_slice(&(donnees.len() as u32 / trame).to_be_bytes());
        comm.extend_from_slice(&bits.to_be_bytes());
        comm.extend_from_slice(&frequence_ext);
        if let Some(c) = aifc {
            comm.extend_from_slice(c);
            comm.extend_from_slice(&[0, 0]);
        }
        let mut ssnd = vec![0u8; 8];
        ssnd.extend_from_slice(donnees);
        let mut corps = Vec::new();
        for (id, c) in [(b"COMM", comm), (b"SSND", ssnd)] {
            corps.extend_from_slice(id);
            corps.extend_from_slice(&(c.len() as u32).to_be_bytes());
            corps.extend_from_slice(&c);
            if c.len() % 2 == 1 {
                corps.push(0);
            }
        }
        let mut v = b"FORM".to_vec();
        v.extend_from_slice(&(4 + corps.len() as u32).to_be_bytes());
        v.extend_from_slice(if aifc.is_some() { b"AIFC" } else { b"AIFF" });
        v.extend_from_slice(&corps);
        v
    }

    /// 44 100 et 192 000 Hz en IEEE 754 étendu 80 bits.
    const EXT_44100: [u8; 10] = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];
    const EXT_192000: [u8; 10] = [0x40, 0x10, 0xBB, 0x80, 0, 0, 0, 0, 0, 0];

    #[test]
    fn aiff_gros_boutiste_et_aifc_sowt() {
        let donnees = echantillons(4 * 12);
        let octets = aiff(2, 16, EXT_44100, &donnees, None);
        let f = ecrire(&octets, ".aiff");
        let e = lire_aiff(f.path()).unwrap();
        assert_eq!(
            (e.frequence, e.bits, e.canaux, e.gros_boutiste),
            (44_100, 16, 2, true)
        );
        verifier_tranche(&octets, &e, &donnees);
        assert_eq!(
            e.champs().unwrap(),
            ChampsPcm {
                taille: b'1',
                frequence: b'3',
                canaux: b'2',
                boutisme: b'0'
            }
        );

        let donnees = echantillons(6 * 12);
        let octets = aiff(2, 24, EXT_192000, &donnees, Some(b"sowt"));
        let f = ecrire(&octets, ".aif");
        let e = lire_aiff(f.path()).unwrap();
        assert!(!e.gros_boutiste, "AIFC sowt : petit-boutiste");
        verifier_tranche(&octets, &e, &donnees);
        assert_eq!(e.champs().unwrap().frequence, b'<');

        let f = ecrire(
            &aiff(2, 16, EXT_44100, &echantillons(48), Some(b"fl32")),
            ".aif",
        );
        assert!(lire_aiff(f.path()).is_err(), "AIFC flottant refusé");
    }
}
