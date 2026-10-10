//! #6059 — servir un fichier NATIF (FLAC, AIFF, DSF) à partir d'une position,
//! sans le convertir.
//!
//! Le Yamaha R-N2000A acquitte un `Seek` SOAP sans l'exécuter (Cyrille, fil
//! 2194). Décision de Bertrand (10/10/2026) : pour avancer dans un fichier
//! natif sur un tel renderer, Tune relance le flux NATIF à partir de l'octet
//! qui correspond à la position demandée. Le bit-perfect est conservé : les
//! octets audio servis sont ceux du fichier, à partir d'une frontière que le
//! format sait reprendre.
//!
//! Le flux servi est un fichier VIRTUEL : un en-tête réécrit, en mémoire, suivi
//! d'une tranche du fichier d'origine. C'est exactement la mécanique de la
//! carte faststart M4A ([`FaststartMap`]) et du conteneur FLAC neuf (#4800),
//! que `serve_file` sait déjà servir, `Range` compris.
//!
//! | format | frontière de reprise                         | en-tête réécrit            |
//! |--------|----------------------------------------------|----------------------------|
//! | FLAC   | trame (code de synchro + CRC-8 de l'en-tête) | STREAMINFO : échantillons restants, MD5 nul ; SEEKTABLE retirée |
//! | AIFF   | trame d'échantillons du bloc `SSND`          | `COMM` : trames restantes ; `SSND` : offset 0 |
//! | DSF    | groupe de blocs (un bloc de 4096 o par canal) | `fmt ` : échantillons restants ; `data` : taille ; pas de métadonnées |
//!
//! La position RÉELLE de départ (la frontière, juste avant la cible) est
//! rendue avec la carte : le renderer compte ensuite sa position à partir de
//! zéro, et la sortie DLNA la rajoute (`outputs::dlna_depart_natif`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::faststart::FaststartMap;

/// Un flux natif qui commence à une position.
#[derive(Clone)]
pub struct DepartNatif {
    /// En-tête réécrit + tranche du fichier, servie par `serve_file`.
    pub carte: FaststartMap,
    /// Position réelle du premier échantillon servi, en millisecondes depuis
    /// le début de la piste. Toujours ≤ la cible demandée.
    pub depart_ms: u64,
}

/// Les formats que ce module sait servir à partir d'une position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatNatif {
    Flac,
    Aiff,
    Dsf,
}

impl FormatNatif {
    /// Le format, d'après l'extension du fichier. `None` pour tout le reste
    /// (DFF, WAV, ALAC… : non traités ici).
    pub fn du_chemin(chemin: &Path) -> Option<Self> {
        let ext = chemin.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "flac" => Some(Self::Flac),
            "aif" | "aiff" | "aifc" => Some(Self::Aiff),
            "dsf" => Some(Self::Dsf),
            _ => None,
        }
    }
}

/// Prépare le flux natif de `chemin` à partir de `position_ms`. `None` si le
/// format n'est pas traité, si le fichier est illisible ou malformé, ou si la
/// position est au-delà de la fin : l'appelant sert alors le fichier entier,
/// comme avant.
pub fn preparer(chemin: &Path, position_ms: u64) -> Option<DepartNatif> {
    if position_ms == 0 {
        return None;
    }
    let mut f = File::open(chemin).ok()?;
    let taille = f.metadata().ok()?.len();
    match FormatNatif::du_chemin(chemin)? {
        FormatNatif::Flac => flac(&mut f, taille, position_ms),
        FormatNatif::Aiff => aiff(&mut f, taille, position_ms),
        FormatNatif::Dsf => dsf(&mut f, taille, position_ms),
    }
}

fn lire_a(f: &mut File, pos: u64, n: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn lire_au_plus(f: &mut File, pos: u64, n: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn ms_vers_echantillons(ms: u64, cadence: u64) -> u64 {
    (ms as u128 * cadence as u128 / 1000) as u64
}

fn echantillons_vers_ms(n: u64, cadence: u64) -> u64 {
    if cadence == 0 {
        0
    } else {
        (n as u128 * 1000 / cadence as u128) as u64
    }
}

// ───────────────────────────── FLAC ─────────────────────────────

/// Ce que dit STREAMINFO.
#[derive(Debug, Clone, Copy)]
struct StreamInfoFlac {
    bloc_min: u16,
    bloc_max: u16,
    cadence: u32,
    total: u64,
}

fn lire_streaminfo(corps: &[u8]) -> Option<StreamInfoFlac> {
    if corps.len() < 34 {
        return None;
    }
    let bloc_min = u16::from_be_bytes([corps[0], corps[1]]);
    let bloc_max = u16::from_be_bytes([corps[2], corps[3]]);
    let cadence =
        ((corps[10] as u32) << 12) | ((corps[11] as u32) << 4) | ((corps[12] as u32) >> 4);
    let total = (((corps[13] & 0x0F) as u64) << 32)
        | ((corps[14] as u64) << 24)
        | ((corps[15] as u64) << 16)
        | ((corps[16] as u64) << 8)
        | (corps[17] as u64);
    Some(StreamInfoFlac {
        bloc_min,
        bloc_max,
        cadence,
        total,
    })
}

/// Réécrit le nombre total d'échantillons de STREAMINFO (36 bits) et met le
/// MD5 à zéro : la signature valait pour la piste ENTIÈRE, et la
/// spécification dit qu'un MD5 nul signifie « inconnu ».
fn reecrire_streaminfo(corps: &mut [u8], total: u64) {
    corps[13] = (corps[13] & 0xF0) | ((total >> 32) as u8 & 0x0F);
    corps[14] = (total >> 24) as u8;
    corps[15] = (total >> 16) as u8;
    corps[16] = (total >> 8) as u8;
    corps[17] = total as u8;
    for b in &mut corps[18..34] {
        *b = 0;
    }
}

fn crc8_flac(octets: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &o in octets {
        crc ^= o;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Ce qui ne change pas d'une trame à l'autre d'un même flux : code de
/// cadence, affectation des canaux et taille d'échantillon. Exigé identique à
/// celui de la première trame, pour qu'un faux code de synchro DANS l'audio
/// (CRC-8 : une chance sur 256) ne passe pas pour une trame.
type SignatureTrame = (u8, u8);

/// L'en-tête de trame FLAC qui commence à `t[0]` est-il valide ? Rend le
/// numéro du premier échantillon de la trame et sa signature.
fn premier_echantillon_de_trame(t: &[u8], info: &StreamInfoFlac) -> Option<(u64, SignatureTrame)> {
    if t.len() < 6 || t[0] != 0xFF || (t[1] & 0xFE) != 0xF8 {
        return None;
    }
    let variable = t[1] & 0x01 == 1;
    let code_bloc = t[2] >> 4;
    let code_cadence = t[2] & 0x0F;
    let canaux = t[3] >> 4;
    let code_taille = (t[3] >> 1) & 0x07;
    if code_bloc == 0 || code_cadence == 0x0F || canaux > 10 || code_taille == 3 || t[3] & 1 != 0 {
        return None;
    }
    // Nombre codé « UTF-8 » (jusqu'à 7 octets).
    let premier = t[4];
    let (mut valeur, suite) = match premier.leading_ones() {
        0 => (premier as u64, 0),
        2..=7 => {
            let n = premier.leading_ones() as usize;
            ((premier & (0xFF >> (n + 1))) as u64, n - 1)
        }
        _ => return None,
    };
    let mut i = 5;
    for _ in 0..suite {
        let o = *t.get(i)?;
        if o & 0xC0 != 0x80 {
            return None;
        }
        valeur = (valeur << 6) | (o & 0x3F) as u64;
        i += 1;
    }
    let taille_bloc: u64 = match code_bloc {
        1 => 192,
        2..=5 => 576 << (code_bloc - 2),
        6 => {
            let v = *t.get(i)? as u64 + 1;
            i += 1;
            v
        }
        7 => {
            let v = u16::from_be_bytes([*t.get(i)?, *t.get(i + 1)?]) as u64 + 1;
            i += 2;
            v
        }
        _ => 256 << (code_bloc - 8),
    };
    match code_cadence {
        12 => i += 1,
        13 | 14 => i += 2,
        _ => {}
    }
    if crc8_flac(t.get(..i)?) != *t.get(i)? {
        return None;
    }
    let signature = (t[2] & 0x0F, t[3]);
    if variable {
        Some((valeur, signature))
    } else {
        // Blocs fixes : le numéro est celui de la TRAME. Toutes les trames
        // sauf la dernière ont la taille de STREAMINFO.
        let fixe = if info.bloc_min == info.bloc_max && info.bloc_max > 0 {
            info.bloc_max as u64
        } else {
            taille_bloc
        };
        Some((valeur * fixe, signature))
    }
}

/// La première trame valide à partir de l'octet `depuis` (au plus `fin`) :
/// `(offset, premier échantillon)`.
fn trame_a_partir_de(
    f: &mut File,
    depuis: u64,
    fin: u64,
    info: &StreamInfoFlac,
    signature: Option<SignatureTrame>,
) -> Option<(u64, u64, SignatureTrame)> {
    const FENETRE: usize = 64 * 1024;
    let mut pos = depuis;
    while pos < fin {
        let n = ((fin - pos) as usize).min(FENETRE + 16);
        let buf = lire_au_plus(f, pos, n)?;
        if buf.len() < 2 {
            return None;
        }
        let bornes = buf.len().saturating_sub(1);
        for i in 0..bornes {
            if buf[i] == 0xFF
                && (buf[i + 1] & 0xFE) == 0xF8
                && let Some((s, sig)) = premier_echantillon_de_trame(&buf[i..], info)
                && (info.total == 0 || s < info.total)
                && signature.is_none_or(|attendue| attendue == sig)
            {
                return Some((pos + i as u64, s, sig));
            }
        }
        if buf.len() < n {
            return None;
        }
        pos += FENETRE as u64;
    }
    None
}

fn flac(f: &mut File, taille: u64, position_ms: u64) -> Option<DepartNatif> {
    if lire_a(f, 0, 4)? != b"fLaC" {
        return None;
    }
    // Les blocs de métadonnées, gardés pour l'en-tête réécrit.
    let mut blocs: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut pos = 4u64;
    loop {
        let h = lire_a(f, pos, 4)?;
        let dernier = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7F;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let corps = lire_a(f, pos + 4, len)?;
        blocs.push((kind, corps));
        pos += 4 + len as u64;
        if dernier {
            break;
        }
        if pos >= taille {
            return None;
        }
    }
    let debut_trames = pos;
    let (kind0, si) = blocs.first()?;
    if *kind0 != 0 {
        return None;
    }
    let info = lire_streaminfo(si)?;
    if info.cadence == 0 {
        return None;
    }
    let cible = ms_vers_echantillons(position_ms, info.cadence as u64);
    // `total == 0` : STREAMINFO ne connaît pas la longueur (encodeur en flux),
    // ce que la spécification permet ; on le laisse inconnu.
    if info.total > 0 && cible >= info.total {
        return None;
    }

    // Recherche dichotomique de la DERNIÈRE trame qui commence à ou avant la
    // cible : sa première trame valide à partir de chaque octet sondé.
    let (premiere_off, premiere_s, sig) = trame_a_partir_de(f, debut_trames, taille, &info, None)?;
    let sig = Some(sig);
    let mut bas = (premiere_off, premiere_s);
    let mut lo = premiere_off;
    let mut hi = taille;
    while hi > lo + 4096 {
        let mid = lo + (hi - lo) / 2;
        match trame_a_partir_de(f, mid, taille, &info, sig) {
            Some((off, s, _)) if s <= cible => {
                if s >= bas.1 {
                    bas = (off, s);
                }
                lo = mid;
            }
            _ => hi = mid,
        }
    }
    // Avance trame par trame jusqu'à la dernière qui ne dépasse pas la cible.
    let mut courant = bas;
    while let Some((off, s, _)) = trame_a_partir_de(f, courant.0 + 2, taille, &info, sig) {
        if s > cible {
            break;
        }
        courant = (off, s);
    }
    let (offset, premier) = courant;

    // En-tête : STREAMINFO réécrit, SEEKTABLE (type 3) retirée — ses offsets
    // désigneraient d'autres trames —, le reste tel quel.
    let restants = info.total.saturating_sub(premier);
    let mut gardes: Vec<(u8, Vec<u8>)> = blocs.into_iter().filter(|(k, _)| *k != 3).collect();
    reecrire_streaminfo(&mut gardes[0].1, restants);
    let mut header = b"fLaC".to_vec();
    let n = gardes.len();
    for (i, (kind, corps)) in gardes.into_iter().enumerate() {
        let drapeau = if i + 1 == n { 0x80 } else { 0 };
        header.push(drapeau | kind);
        let len = corps.len() as u32;
        header.extend_from_slice(&len.to_be_bytes()[1..]);
        header.extend_from_slice(&corps);
    }
    let body_len = taille - offset;
    Some(DepartNatif {
        carte: FaststartMap {
            total: header.len() as u64 + body_len,
            header,
            body_src_start: offset,
            body_len,
        },
        depart_ms: echantillons_vers_ms(premier, info.cadence as u64),
    })
}

// ───────────────────────────── AIFF ─────────────────────────────

/// Un flottant IEEE 754 étendu 80 bits (cadence AIFF).
fn etendu_80(b: &[u8]) -> f64 {
    let expo = (((b[0] & 0x7F) as i32) << 8) | b[1] as i32;
    let mantisse = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if expo == 0 && mantisse == 0 {
        return 0.0;
    }
    mantisse as f64 * 2f64.powi(expo - 16383 - 63)
}

fn aiff(f: &mut File, taille: u64, position_ms: u64) -> Option<DepartNatif> {
    let tete = lire_a(f, 0, 12)?;
    if &tete[0..4] != b"FORM" {
        return None;
    }
    let aifc = match &tete[8..12] {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return None,
    };
    // Morceaux AVANT `SSND`, recopiés (COMM patché) ; ceux d'après (ID3,
    // souvent) sont laissés : ils ne portent pas d'audio.
    let mut avant: Vec<u8> = Vec::new();
    let mut comm: Option<(usize, u16, u32, u16, f64)> = None; // (position dans `avant`, canaux, trames, bits, cadence)
    let mut pos = 12u64;
    let (ssnd_donnees, ssnd_fin) = loop {
        if pos + 8 > taille {
            return None;
        }
        let h = lire_a(f, pos, 8)?;
        let id = [h[0], h[1], h[2], h[3]];
        let len = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as u64;
        let pad = len & 1;
        if &id == b"SSND" {
            let s = lire_a(f, pos + 8, 8)?;
            let offset = u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as u64;
            let debut = pos + 16 + offset;
            let fin = (pos + 8 + len).min(taille);
            break (debut, fin);
        }
        let corps = lire_a(f, pos + 8, len as usize)?;
        if &id == b"COMM" {
            if corps.len() < 18 {
                return None;
            }
            if aifc {
                let compression = corps.get(18..22)?;
                if !matches!(compression, b"NONE" | b"twos" | b"sowt" | b"raw ") {
                    return None;
                }
            }
            comm = Some((
                avant.len(),
                u16::from_be_bytes([corps[0], corps[1]]),
                u32::from_be_bytes([corps[2], corps[3], corps[4], corps[5]]),
                u16::from_be_bytes([corps[6], corps[7]]),
                etendu_80(&corps[8..18]),
            ));
        }
        avant.extend_from_slice(&h);
        avant.extend_from_slice(&corps);
        if pad == 1 {
            avant.push(0);
        }
        pos += 8 + len + pad;
    };
    let (pos_comm, canaux, trames, bits, cadence) = comm?;
    let cadence = cadence.round() as u64;
    let trame = canaux as u64 * (bits as u64).div_ceil(8);
    if cadence == 0 || trame == 0 || trames == 0 {
        return None;
    }
    let premiere = ms_vers_echantillons(position_ms, cadence);
    if premiere >= trames as u64 {
        return None;
    }
    let debut = ssnd_donnees + premiere * trame;
    let fin = ssnd_fin.min(ssnd_donnees + trames as u64 * trame);
    if debut >= fin {
        return None;
    }
    let restantes = trames as u64 - premiere;
    // COMM : nombre de trames (octets 2..6 du corps, après 8 d'en-tête).
    avant[pos_comm + 10..pos_comm + 14].copy_from_slice(&(restantes as u32).to_be_bytes());
    let body_len = fin - debut;
    let ssnd_len = 8 + body_len;
    let mut header = b"FORM".to_vec();
    let form_len = 4 + avant.len() as u64 + 8 + ssnd_len;
    header.extend_from_slice(&(form_len as u32).to_be_bytes());
    header.extend_from_slice(if aifc { b"AIFC" } else { b"AIFF" });
    header.extend_from_slice(&avant);
    header.extend_from_slice(b"SSND");
    header.extend_from_slice(&(ssnd_len as u32).to_be_bytes());
    header.extend_from_slice(&[0u8; 8]); // offset 0, blockSize 0
    Some(DepartNatif {
        carte: FaststartMap {
            total: header.len() as u64 + body_len,
            header,
            body_src_start: debut,
            body_len,
        },
        depart_ms: echantillons_vers_ms(premiere, cadence),
    })
}

// ───────────────────────────── DSF ─────────────────────────────

fn dsf(f: &mut File, taille: u64, position_ms: u64) -> Option<DepartNatif> {
    let dsd = lire_a(f, 0, 28)?;
    if &dsd[0..4] != b"DSD " {
        return None;
    }
    let dsd_len = u64::from_le_bytes(dsd[4..12].try_into().ok()?);
    let fmt_h = lire_a(f, dsd_len, 12)?;
    if &fmt_h[0..4] != b"fmt " {
        return None;
    }
    let fmt_len = u64::from_le_bytes(fmt_h[4..12].try_into().ok()?);
    let mut fmt = lire_a(f, dsd_len, fmt_len as usize)?;
    if fmt.len() < 52 {
        return None;
    }
    let canaux = u32::from_le_bytes(fmt[24..28].try_into().ok()?) as u64;
    let cadence = u32::from_le_bytes(fmt[28..32].try_into().ok()?) as u64;
    let bits = u32::from_le_bytes(fmt[32..36].try_into().ok()?);
    let echantillons = u64::from_le_bytes(fmt[36..44].try_into().ok()?);
    let bloc = u32::from_le_bytes(fmt[44..48].try_into().ok()?) as u64;
    if canaux == 0 || cadence == 0 || bloc == 0 || bits != 1 || echantillons == 0 {
        return None;
    }
    let data_pos = dsd_len + fmt_len;
    let data_h = lire_a(f, data_pos, 12)?;
    if &data_h[0..4] != b"data" {
        return None;
    }
    let data_len = u64::from_le_bytes(data_h[4..12].try_into().ok()?);
    let donnees = data_pos + 12;
    let fin = (data_pos + data_len).min(taille);
    // Un groupe = un bloc par canal ; un bloc porte `bloc × 8` échantillons
    // d'un canal (1 bit par échantillon).
    let par_groupe = bloc * 8;
    let groupe_octets = bloc * canaux;
    let cible = ms_vers_echantillons(position_ms, cadence);
    if cible >= echantillons {
        return None;
    }
    let groupe = cible / par_groupe;
    let debut = donnees + groupe * groupe_octets;
    if debut >= fin {
        return None;
    }
    let premier = groupe * par_groupe;
    let restants = echantillons - premier;
    let body_len = fin - debut;
    fmt[36..44].copy_from_slice(&restants.to_le_bytes());
    let mut header = Vec::with_capacity(28 + fmt.len() + 12);
    header.extend_from_slice(b"DSD ");
    header.extend_from_slice(&28u64.to_le_bytes());
    let total = 28 + fmt.len() as u64 + 12 + body_len;
    header.extend_from_slice(&total.to_le_bytes());
    header.extend_from_slice(&0u64.to_le_bytes()); // pas de métadonnées
    header.extend_from_slice(&fmt);
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(12 + body_len).to_le_bytes());
    Some(DepartNatif {
        carte: FaststartMap {
            total: header.len() as u64 + body_len,
            header,
            body_src_start: debut,
            body_len,
        },
        depart_ms: echantillons_vers_ms(premier, cadence),
    })
}

#[cfg(test)]
#[path = "depart_natif_tests_6059.rs"]
mod tests;
