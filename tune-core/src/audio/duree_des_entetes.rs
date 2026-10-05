//! La DURÉE d'un fichier audio lue dans ses seuls EN-TÊTES (fil 2062).
//!
//! Un serveur UPnP qui n'annonce aucune `res@duration` — la Freebox, mesurée
//! par le fil 2062 : barre de lecture à 0:00 alors que le temps écoulé avance
//! — laisse Tune sans durée. Le fichier, lui, la porte presque toujours dans
//! ses premiers octets :
//!
//! - **FLAC** : STREAMINFO, total d'échantillons / fréquence ;
//! - **MP3** : en-tête Xing/Info ou VBRI (nombre de trames), à défaut le débit
//!   constant de la première trame rapporté à la taille du fichier ;
//! - **MP4 / M4A** : l'atome `mvhd` (durée / échelle de temps), que `moov`
//!   soit en tête ou après `mdat` ;
//! - **WAV** : taille du bloc `data` / débit d'octets du bloc `fmt `.
//!
//! Ce module ne fait AUCUNE entrée-sortie : il lit un tampon qui commence à un
//! décalage connu du fichier, et dit soit la durée, soit quels octets lire
//! ensuite (balise ID3v2 plus longue que le tampon, `moov` placé en fin de
//! fichier). L'orchestrateur fait les requêtes `Range`.

/// Ce que la lecture d'un tampon d'en-tête conclut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lecture {
    /// La durée, en millisecondes, strictement positive.
    Duree(u64),
    /// Il faut lire `longueur` octets à partir de `debut` puis reprendre avec
    /// [`analyser_la_suite`] et la `suite` donnée.
    Lire {
        debut: u64,
        longueur: u64,
        suite: Suite,
    },
    /// Rien d'exploitable : format inconnu, en-tête tronqué ou durée nulle.
    Inconnue,
}

/// Ce qu'on sait du fichier quand il faut lire plus loin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    /// Après une balise ID3v2 : le format reste à reconnaître.
    ApresId3,
    /// Un MP4 dont on parcourt les atomes de premier niveau.
    AtomesMp4,
}

/// Taille d'une lecture de reprise (atome `moov`, trame MP3 après ID3).
const LECTURE_DE_REPRISE: u64 = 64 * 1024;

/// Analyse le DÉBUT du fichier (`tete` commence à l'octet 0).
///
/// `taille_totale` est la taille du fichier quand le serveur l'a dite
/// (`Content-Range` ou `Content-Length`) : elle sert au MP3 à débit constant,
/// au WAV dont le bloc `data` ne dit pas sa taille, et à borner les sauts MP4.
pub fn analyser_la_tete(tete: &[u8], taille_totale: Option<u64>) -> Lecture {
    analyser(tete, 0, taille_totale, None)
}

/// Reprend l'analyse sur un tampon lu à `debut` à la demande d'une
/// [`Lecture::Lire`].
pub fn analyser_la_suite(
    octets: &[u8],
    debut: u64,
    taille_totale: Option<u64>,
    suite: Suite,
) -> Lecture {
    match suite {
        Suite::ApresId3 => analyser(octets, debut, taille_totale, Some(debut)),
        Suite::AtomesMp4 => atomes_mp4(octets, debut, taille_totale),
    }
}

fn analyser(buf: &[u8], base: u64, total: Option<u64>, apres_id3: Option<u64>) -> Lecture {
    if buf.len() >= 10 && &buf[..3] == b"ID3" {
        let taille = taille_id3v2(buf);
        let fin = base + taille;
        if (taille as usize) < buf.len() {
            return analyser(&buf[taille as usize..], fin, total, Some(fin));
        }
        if total.is_some_and(|t| fin >= t) {
            return Lecture::Inconnue;
        }
        return Lecture::Lire {
            debut: fin,
            longueur: LECTURE_DE_REPRISE,
            suite: Suite::ApresId3,
        };
    }
    if buf.len() >= 4 && &buf[..4] == b"fLaC" {
        return flac(buf);
    }
    if buf.len() >= 12 && &buf[..4] == b"RIFF" && &buf[8..12] == b"WAVE" {
        return wav(buf, base, total);
    }
    if buf.len() >= 8 && &buf[4..8] == b"ftyp" {
        return atomes_mp4(buf, base, total);
    }
    mp3(buf, base, total, apres_id3.unwrap_or(base))
}

fn ms(numerateur: u128, denominateur: u128) -> Lecture {
    if denominateur == 0 {
        return Lecture::Inconnue;
    }
    let d = (numerateur * 1000 / denominateur) as u64;
    if d == 0 {
        Lecture::Inconnue
    } else {
        Lecture::Duree(d)
    }
}

fn be32(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], i: usize) -> Option<u64> {
    b.get(i..i + 8).map(|s| {
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        u64::from_be_bytes(a)
    })
}

fn le32(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Longueur totale d'une balise ID3v2 (en-tête, corps et pied éventuel).
fn taille_id3v2(b: &[u8]) -> u64 {
    let corps = ((b[6] as u64 & 0x7F) << 21)
        | ((b[7] as u64 & 0x7F) << 14)
        | ((b[8] as u64 & 0x7F) << 7)
        | (b[9] as u64 & 0x7F);
    let pied = if b[5] & 0x10 != 0 { 10 } else { 0 };
    10 + corps + pied
}

// ─── FLAC ───────────────────────────────────────────────────────────────────

fn flac(b: &[u8]) -> Lecture {
    // Le premier bloc de métadonnées est TOUJOURS STREAMINFO (34 octets).
    let Some(si) = b.get(8..8 + 34) else {
        return Lecture::Inconnue;
    };
    if b[4] & 0x7F != 0 {
        return Lecture::Inconnue;
    }
    let frequence = ((si[10] as u32) << 12) | ((si[11] as u32) << 4) | ((si[12] as u32) >> 4);
    let echantillons = ((si[13] as u64 & 0x0F) << 32)
        | ((si[14] as u64) << 24)
        | ((si[15] as u64) << 16)
        | ((si[16] as u64) << 8)
        | si[17] as u64;
    ms(echantillons as u128, frequence as u128)
}

// ─── WAV ────────────────────────────────────────────────────────────────────

fn wav(b: &[u8], base: u64, total: Option<u64>) -> Lecture {
    let mut i = 12usize;
    let mut debit_octets: Option<u32> = None;
    while let (Some(id), Some(taille)) = (b.get(i..i + 4), le32(b, i + 4)) {
        let corps = i + 8;
        match id {
            b"fmt " => debit_octets = le32(b, corps + 8),
            b"data" => {
                let Some(debit) = debit_octets.filter(|d| *d > 0) else {
                    return Lecture::Inconnue;
                };
                let reste = total.map(|t| t.saturating_sub(base + corps as u64));
                // Un WAV servi en flux porte souvent 0 ou 0xFFFFFFFF ici : la
                // taille du fichier dit alors la vérité.
                let donnees = if taille == 0 || taille == u32::MAX {
                    match reste {
                        Some(r) => r,
                        None => return Lecture::Inconnue,
                    }
                } else {
                    let t = taille as u64;
                    reste.map_or(t, |r| t.min(r))
                };
                return ms(donnees as u128, debit as u128);
            }
            _ => {}
        }
        i = corps + taille as usize + (taille as usize & 1);
    }
    Lecture::Inconnue
}

// ─── MP4 / M4A ──────────────────────────────────────────────────────────────

/// Parcourt les atomes de premier niveau de `b`, qui commence à l'octet `base`
/// du fichier sur une frontière d'atome.
fn atomes_mp4(b: &[u8], base: u64, total: Option<u64>) -> Lecture {
    let mut i = 0usize;
    while let (Some(taille32), Some(genre)) = (be32(b, i), b.get(i + 4..i + 8)) {
        let (taille, entete) = match taille32 {
            1 => match be64(b, i + 8) {
                Some(t) => (t, 16u64),
                None => break,
            },
            0 => match total {
                Some(t) => (t.saturating_sub(base + i as u64), 8),
                None => return Lecture::Inconnue,
            },
            t => (t as u64, 8),
        };
        if taille < entete {
            return Lecture::Inconnue;
        }
        if genre == b"moov" {
            let debut_corps = i + entete as usize;
            let fin = (i as u64 + taille).min(b.len() as u64) as usize;
            return match b.get(debut_corps..fin).and_then(mvhd_dans_moov) {
                Some(l) => l,
                // `moov` tronqué : on relit à partir de lui.
                None if fin < (i as u64 + taille) as usize && i > 0 => Lecture::Lire {
                    debut: base + i as u64,
                    longueur: LECTURE_DE_REPRISE,
                    suite: Suite::AtomesMp4,
                },
                None => Lecture::Inconnue,
            };
        }
        let suivant = i as u64 + taille;
        if suivant >= b.len() as u64 {
            let debut = base + suivant;
            if total.is_some_and(|t| debut >= t) {
                return Lecture::Inconnue;
            }
            return Lecture::Lire {
                debut,
                longueur: LECTURE_DE_REPRISE,
                suite: Suite::AtomesMp4,
            };
        }
        i = suivant as usize;
    }
    Lecture::Inconnue
}

fn mvhd_dans_moov(corps: &[u8]) -> Option<Lecture> {
    let mut i = 0usize;
    while let (Some(taille), Some(genre)) = (be32(corps, i), corps.get(i + 4..i + 8)) {
        if genre == b"mvhd" {
            let v = *corps.get(i + 8)?;
            let p = i + 12;
            let (echelle, duree) = if v == 1 {
                (be32(corps, p + 16)? as u64, be64(corps, p + 20)?)
            } else {
                let d = be32(corps, p + 12)?;
                (
                    be32(corps, p + 8)? as u64,
                    if d == u32::MAX { u64::MAX } else { d as u64 },
                )
            };
            if duree == u64::MAX {
                return Some(Lecture::Inconnue);
            }
            return Some(ms(duree as u128, echelle as u128));
        }
        if taille < 8 {
            return Some(Lecture::Inconnue);
        }
        i += taille as usize;
    }
    None
}

// ─── MP3 ────────────────────────────────────────────────────────────────────

const DEBITS_MPEG1: [[u32; 15]; 3] = [
    [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ],
    [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ],
];
const DEBITS_MPEG2: [[u32; 15]; 2] = [
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

struct TrameMp3 {
    mpeg1: bool,
    mono: bool,
    debit_kbps: u32,
    frequence: u32,
    echantillons: u32,
    longueur: usize,
}

fn trame_mp3(h: &[u8]) -> Option<TrameMp3> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let couche = (h[1] >> 1) & 3; // 1 = III, 2 = II, 3 = I
    if version == 1 || couche == 0 {
        return None;
    }
    let indice_debit = (h[2] >> 4) as usize;
    let indice_freq = ((h[2] >> 2) & 3) as usize;
    if indice_debit == 0 || indice_debit == 15 || indice_freq == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    let frequence = [44_100, 48_000, 32_000][indice_freq]
        / match version {
            3 => 1,
            2 => 2,
            _ => 4,
        };
    let debit_kbps = if mpeg1 {
        DEBITS_MPEG1[(3 - couche) as usize][indice_debit]
    } else {
        DEBITS_MPEG2[if couche == 3 { 0 } else { 1 }][indice_debit]
    };
    let rembourrage = ((h[2] >> 1) & 1) as usize;
    let (echantillons, longueur) = match couche {
        3 => (
            384,
            (12 * debit_kbps as usize * 1000 / frequence as usize + rembourrage) * 4,
        ),
        2 => (
            1152,
            144 * debit_kbps as usize * 1000 / frequence as usize + rembourrage,
        ),
        _ if mpeg1 => (
            1152,
            144 * debit_kbps as usize * 1000 / frequence as usize + rembourrage,
        ),
        _ => (
            576,
            72 * debit_kbps as usize * 1000 / frequence as usize + rembourrage,
        ),
    };
    Some(TrameMp3 {
        mpeg1,
        mono: h[3] >> 6 == 3,
        debit_kbps,
        frequence,
        echantillons,
        longueur,
    })
}

fn mp3(b: &[u8], base: u64, total: Option<u64>, debut_audio_min: u64) -> Lecture {
    // La première trame : synchro valide ET, quand le tampon la contient, une
    // seconde synchro exactement une trame plus loin — un 0xFFE isolé dans
    // des octets quelconques ne suffit pas à conclure.
    let fenetre = b.len().min(16 * 1024);
    let mut trouve = None;
    for i in 0..fenetre.saturating_sub(4) {
        if let Some(t) = trame_mp3(&b[i..]) {
            let j = i + t.longueur;
            if j + 4 > b.len() || trame_mp3(&b[j..]).is_some() {
                trouve = Some((i, t));
                break;
            }
        }
    }
    let Some((i, t)) = trouve else {
        return Lecture::Inconnue;
    };
    let trame = &b[i..];
    let decalage_xing = match (t.mpeg1, t.mono) {
        (true, false) => 36,
        (true, true) => 21,
        (false, false) => 21,
        (false, true) => 13,
    };
    // Xing / Info : nombre de trames quand le drapeau 1 est posé.
    if let Some(tag) = trame.get(decalage_xing..decalage_xing + 4)
        && (tag == b"Xing" || tag == b"Info")
        && let Some(drapeaux) = be32(trame, decalage_xing + 4)
        && drapeaux & 1 != 0
        && let Some(trames) = be32(trame, decalage_xing + 8)
        && trames > 0
    {
        return ms(trames as u128 * t.echantillons as u128, t.frequence as u128);
    }
    // VBRI (Fraunhofer) : toujours 32 octets après l'en-tête de 4.
    if trame.get(36..40) == Some(b"VBRI")
        && let Some(trames) = be32(trame, 36 + 14)
        && trames > 0
    {
        return ms(trames as u128 * t.echantillons as u128, t.frequence as u128);
    }
    // Débit constant : la taille de l'audio au débit de la première trame.
    let Some(total) = total else {
        return Lecture::Inconnue;
    };
    let debut_audio = (base + i as u64).max(debut_audio_min);
    let octets = total.saturating_sub(debut_audio);
    ms(octets as u128 * 8, t.debit_kbps as u128 * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un FLAC minimal : `fLaC` + STREAMINFO.
    fn flac_de(frequence: u32, echantillons: u64) -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut si = [0u8; 34];
        si[10] = (frequence >> 12) as u8;
        si[11] = (frequence >> 4) as u8;
        si[12] = ((frequence & 0x0F) << 4) as u8 | (1 << 1); // 2 canaux
        si[13] = (15 << 4) | ((echantillons >> 32) as u8 & 0x0F); // 16 bits
        si[14..18].copy_from_slice(&(echantillons as u32).to_be_bytes());
        v.extend_from_slice(&si);
        v
    }

    #[test]
    fn flac_streaminfo() {
        let b = flac_de(44_100, 44_100 * 273);
        assert_eq!(analyser_la_tete(&b, None), Lecture::Duree(273_000));
    }

    #[test]
    fn flac_sans_total_d_echantillons_est_inconnu() {
        let b = flac_de(44_100, 0);
        assert_eq!(analyser_la_tete(&b, None), Lecture::Inconnue);
    }

    #[test]
    fn wav_bloc_data() {
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&44_100u32.to_le_bytes());
        b.extend_from_slice(&176_400u32.to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(176_400u32 * 10).to_le_bytes());
        assert_eq!(analyser_la_tete(&b, None), Lecture::Duree(10_000));
    }

    #[test]
    fn id3_plus_long_que_le_tampon_demande_la_suite() {
        let mut b = b"ID3\x04\0\0".to_vec();
        b.extend_from_slice(&[0, 0x10, 0, 0]); // 0x10 << 14 = 262144 octets
        b.resize(1024, 0);
        assert_eq!(
            analyser_la_tete(&b, Some(10_000_000)),
            Lecture::Lire {
                debut: 10 + 262_144,
                longueur: LECTURE_DE_REPRISE,
                suite: Suite::ApresId3
            }
        );
    }

    #[test]
    fn octets_quelconques_sont_inconnus() {
        assert_eq!(
            analyser_la_tete(b"<html>pas un son</html>", Some(23)),
            Lecture::Inconnue
        );
    }
}
