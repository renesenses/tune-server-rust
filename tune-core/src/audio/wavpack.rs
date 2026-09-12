//! Native WavPack (.wv) lossless decoder — pure Rust.
//!
//! Supports:
//! - WavPack version 4.x lossless (no hybrid/lossy)
//! - 8/16/24/32-bit integer samples
//! - Mono and stereo
//! - All standard sample rates (6 kHz – 192 kHz)
//! - Decorrelation (terms 1-8, 17, 18, -1, -2, -3)
//! - Joint stereo
//! - Adaptive entropy coding (3-median Golomb/Rice)

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use tracing::{debug, warn};

use super::decode::DecodedAudio;

// ── Constants ──────────────────────────────────────────────────────────

const WAVPACK_MAGIC: [u8; 4] = *b"wvpk";

const SAMPLE_RATES: [u32; 16] = [
    6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
    176400, 192000,
];

// Flag bit masks
const FLAG_BYTES_PER_SAMPLE_MASK: u32 = 0x03; // bits 0-1
const FLAG_MONO: u32 = 1 << 2;
const FLAG_HYBRID: u32 = 1 << 3;
const FLAG_JOINT_STEREO: u32 = 1 << 4;
const _FLAG_CROSS_DECORR: u32 = 1 << 5;
// 🔴 Deux valeurs fausses jusqu'ici, mesurées contre `include/wavpack.h` de
// WavPack 5.6.0 :
//   * FALSE_STEREO vaut 0x4000_0000 (bit 30), pas `1 << 27` (bit inutilisé) —
//     un bloc mono déguisé en stéréo était donc décodé comme du vrai stéréo,
//     et le canal droit sortait du néant.
//   * DSD_FLAG vaut 0x8000_0000 (bit 31) ; `1 << 29` est NEW_SHAPING. Le refus
//     « DSD WavPack not supported » ne se déclenchait JAMAIS : un .wv DSD
//     partait dans le décodeur PCM, c'est-à-dire en bruit.
const FLAG_FALSE_STEREO: u32 = 0x4000_0000;
const FLAG_DSD: u32 = 0x8000_0000;
const FLAG_INITIAL_BLOCK: u32 = 1 << 11;
const FLAG_FINAL_BLOCK: u32 = 1 << 12;
const _FLAG_EXTENDED_INT: u32 = 1 << 8;
// 🔴 SHIFT_MASK vaut `0x1f << 13` dans le format (cinq bits, décalage de 0 à
// 31). Le masque de DEUX bits utilisé jusqu'ici tronquait tout décalage ≥ 4 :
// un 24 bits stocké en 20 bits + shift 4 sortait 16 fois trop bas.
const FLAG_LEFT_SHIFT_MASK: u32 = 0x1F << 13; // bits 13-17
const FLAG_SAMPLE_RATE_MASK: u32 = 0x0F << 23; // bits 23-26

// Sub-block IDs (low 5 bits)
const SUB_DECORR_TERMS: u8 = 0x02;
const SUB_DECORR_WEIGHTS: u8 = 0x03;
const SUB_DECORR_SAMPLES: u8 = 0x04;
const SUB_ENTROPY_VARS: u8 = 0x05;
const SUB_BITSTREAM: u8 = 0x0A;
const SUB_WVX_BITSTREAM: u8 = 0x0C;
const SUB_INT32_INFO: u8 = 0x09;
const SUB_CHANNEL_INFO: u8 = 0x0D;
const SUB_SAMPLE_RATE: u8 = 0x27; // non-standard rate (ID with ODD flag = 0x07 | 0x20)

// ── Public types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WavPackInfo {
    pub channels: u32,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
}

// ── Block header ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BlockHeader {
    block_size: u32,
    version: u16,
    total_samples: u32,
    block_index: u32,
    block_samples: u32,
    flags: u32,
    crc: u32,
}

impl BlockHeader {
    fn bytes_per_sample(&self) -> u32 {
        (self.flags & FLAG_BYTES_PER_SAMPLE_MASK) + 1
    }

    fn bits_per_sample(&self) -> u32 {
        self.bytes_per_sample() * 8
    }

    fn is_mono(&self) -> bool {
        self.flags & FLAG_MONO != 0
    }

    /// `MONO_DATA` = `MONO_FLAG | FALSE_STEREO` : le bloc ne porte qu'un canal
    /// de données, qu'il soit annoncé mono ou « stéréo faux ».
    fn is_mono_data(&self) -> bool {
        self.flags & (FLAG_MONO | FLAG_FALSE_STEREO) != 0
    }

    fn is_hybrid(&self) -> bool {
        self.flags & FLAG_HYBRID != 0
    }

    fn is_joint_stereo(&self) -> bool {
        self.flags & FLAG_JOINT_STEREO != 0
    }

    fn is_false_stereo(&self) -> bool {
        self.flags & FLAG_FALSE_STEREO != 0
    }

    fn is_dsd(&self) -> bool {
        self.flags & FLAG_DSD != 0
    }

    #[allow(dead_code)]
    fn is_initial_block(&self) -> bool {
        self.flags & FLAG_INITIAL_BLOCK != 0
    }

    #[allow(dead_code)]
    fn is_final_block(&self) -> bool {
        self.flags & FLAG_FINAL_BLOCK != 0
    }

    fn left_shift(&self) -> u32 {
        (self.flags & FLAG_LEFT_SHIFT_MASK) >> 13
    }

    fn sample_rate_index(&self) -> usize {
        ((self.flags & FLAG_SAMPLE_RATE_MASK) >> 23) as usize
    }

    fn sample_rate(&self) -> u32 {
        let idx = self.sample_rate_index();
        if idx == 15 {
            // 15 = unknown / stored in metadata sub-block
            0
        } else {
            SAMPLE_RATES[idx]
        }
    }

    #[cfg(test)]
    fn channels(&self) -> u32 {
        if self.is_mono() { 1 } else { 2 }
    }
}

fn read_u16_le(r: &mut impl Read) -> Result<u16, String> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read u16: {e}"))?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32_le(r: &mut impl Read) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)
        .map_err(|e| format!("read u32: {e}"))?;
    Ok(u32::from_le_bytes(buf))
}

fn read_block_header(r: &mut impl Read) -> Result<BlockHeader, String> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)
        .map_err(|e| format!("read magic: {e}"))?;
    if magic != WAVPACK_MAGIC {
        return Err(format!(
            "not a WavPack block (magic: {:02x}{:02x}{:02x}{:02x})",
            magic[0], magic[1], magic[2], magic[3]
        ));
    }

    let block_size = read_u32_le(r)?;
    let version = read_u16_le(r)?;
    let _track = {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)
            .map_err(|e| format!("read track: {e}"))?;
        b[0]
    };
    let _index = {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)
            .map_err(|e| format!("read index: {e}"))?;
        b[0]
    };
    let total_samples = read_u32_le(r)?;
    let block_index = read_u32_le(r)?;
    let block_samples = read_u32_le(r)?;
    let flags = read_u32_le(r)?;
    let crc = read_u32_le(r)?;

    Ok(BlockHeader {
        block_size,
        version,
        total_samples,
        block_index,
        block_samples,
        flags,
        crc,
    })
}

// ── Sub-block parsing ──────────────────────────────────────────────────

#[derive(Debug)]
struct SubBlock {
    id: u8,
    data: Vec<u8>,
}

fn parse_sub_blocks(data: &[u8]) -> Vec<SubBlock> {
    let mut blocks = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        if pos >= data.len() {
            break;
        }
        let id_byte = data[pos];
        pos += 1;

        let is_large = id_byte & 0x80 != 0;
        let is_odd_size = id_byte & 0x40 != 0;
        let sub_id = id_byte & 0x3F;

        let word_size = if is_large {
            if pos + 3 > data.len() {
                break;
            }
            let sz = data[pos] as u32 | (data[pos + 1] as u32) << 8 | (data[pos + 2] as u32) << 16;
            pos += 3;
            sz
        } else {
            if pos >= data.len() {
                break;
            }
            let sz = data[pos] as u32;
            pos += 1;
            sz
        };

        let byte_size = (word_size * 2) as usize;
        let actual_size = if is_odd_size && byte_size > 0 {
            byte_size - 1
        } else {
            byte_size
        };

        if pos + byte_size > data.len() {
            break;
        }

        let sub_data = data[pos..pos + actual_size].to_vec();
        pos += byte_size; // advance by full word-aligned size

        blocks.push(SubBlock {
            id: sub_id,
            data: sub_data,
        });
    }

    blocks
}

// ── Entropy decoding (adaptive Golomb/Rice with 3 medians) ─────────────
//
// Tout ce qui suit — modèle à trois médianes, report `holding_one` /
// `holding_zero`, séries de zéros, `read_code`, `exp2s`, passes de
// décorrélation, mixage joint stereo, CRC de bloc — est un portage fidèle du
// décodeur de référence WavPack 5.6.0 (`read_words.c`, `unpack.c`,
// `decorr_utils.c`, `entropy_utils.c`) :
//
//   Copyright (c) 1998 - 2022 David Bryant / Conifer Software.
//   Distribué sous licence BSD à 3 clauses (fichier COPYING de WavPack).
//
// La version précédente de ce module était une reconstitution de tête, non
// validée sur un seul fichier réel : elle décodait du bruit à pleine échelle
// (#3849). Ne pas « simplifier » ces fonctions : chaque décalage et chaque
// arrondi est celui du format, pas un choix d'écriture.

const LIMIT_ONES: u32 = 16;
const MAX_TERM: i32 = 8;

/// `exp2_table` de WavPack : `exp2_table[i] | 0x100 == round(256 * 2^(i/256))`.
const EXP2_TABLE: [u8; 256] = [
    0x00, 0x01, 0x01, 0x02, 0x03, 0x03, 0x04, 0x05, 0x06, 0x06, 0x07, 0x08, 0x08, 0x09, 0x0a, 0x0b,
    0x0b, 0x0c, 0x0d, 0x0e, 0x0e, 0x0f, 0x10, 0x10, 0x11, 0x12, 0x13, 0x13, 0x14, 0x15, 0x16, 0x16,
    0x17, 0x18, 0x19, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1d, 0x1e, 0x1f, 0x20, 0x20, 0x21, 0x22, 0x23,
    0x24, 0x24, 0x25, 0x26, 0x27, 0x28, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2c, 0x2d, 0x2e, 0x2f, 0x30,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3a, 0x3b, 0x3c, 0x3d,
    0x3e, 0x3f, 0x40, 0x41, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x48, 0x49, 0x4a, 0x4b,
    0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a,
    0x5b, 0x5c, 0x5d, 0x5e, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79,
    0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x87, 0x88, 0x89, 0x8a,
    0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b,
    0x9c, 0x9d, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad,
    0xaf, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0,
    0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc8, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf, 0xd0, 0xd2, 0xd3, 0xd4,
    0xd6, 0xd7, 0xd8, 0xd9, 0xdb, 0xdc, 0xdd, 0xde, 0xe0, 0xe1, 0xe2, 0xe4, 0xe5, 0xe6, 0xe8, 0xe9,
    0xea, 0xec, 0xed, 0xee, 0xf0, 0xf1, 0xf2, 0xf4, 0xf5, 0xf6, 0xf8, 0xf9, 0xfa, 0xfc, 0xfd, 0xff,
];

/// `wp_exp2s` de WavPack. **Le logarithme est SIGNÉ** : les échantillons de
/// décorrélation sont stockés en `int16_t`, et un log négatif se décode en
/// `-exp2s(-log)`. L'ancienne version prenait le bit 15 pour un signe et les
/// bits 8-14 pour un exposant, d'où un décalage de plus de 100 rangs (panique
/// en débogage, valeur arbitraire en production).
fn exp2s(log: i32) -> i32 {
    if log < 0 {
        // `wrapping_neg` : un log de -32768 (borne d'un `int16_t`) donne
        // 0x8000_0000, dont la negation deborde. Le C de reference deborde
        // pareil et retombe sur la meme valeur ; on ne panique pas ici.
        return exp2s(log.wrapping_neg()).wrapping_neg();
    }

    let value = (EXP2_TABLE[(log & 0xff) as usize] as u32) | 0x100;
    let log = log >> 8;

    if log <= 9 {
        (value >> (9 - log)) as i32
    } else {
        (value << ((log - 9) & 0x1f)) as i32
    }
}

/// `count_bits` de WavPack : rang du bit de poids fort, +1.
#[inline]
fn count_bits(v: u32) -> u32 {
    if v == 0 { 0 } else { 32 - v.leading_zeros() }
}

/// Bitstream reader for the compressed audio data.
struct BitstreamReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u32, // 0-7, bit within current byte (LSB first)
    /// Vrai dès qu'on a lu au-delà de la fin du flux. Le décodeur de
    /// référence « enroule » ; ici on rend des zéros et on le SIGNALE, pour
    /// que le bloc soit refusé au lieu de produire du bruit.
    overrun: bool,
}

impl<'a> BitstreamReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
            overrun: false,
        }
    }

    fn read_bit(&mut self) -> Option<u32> {
        if self.byte_pos >= self.data.len() {
            self.overrun = true;
            return None;
        }
        let bit = ((self.data[self.byte_pos] >> self.bit_pos) & 1) as u32;
        self.bit_pos += 1;
        if self.bit_pos >= 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit)
    }

    /// `getbit` : hors flux, rend 0 et arme `overrun`.
    #[inline]
    fn getbit(&mut self) -> u32 {
        self.read_bit().unwrap_or(0)
    }

    fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for i in 0..n {
            value |= self.read_bit()? << i;
        }
        Some(value)
    }

    /// `getbits` : hors flux, complète par des zéros et arme `overrun`.
    #[inline]
    fn getbits(&mut self, n: u32) -> u32 {
        let mut value = 0u32;
        for i in 0..n {
            value |= self.getbit() << i;
        }
        value
    }

    /// Count consecutive zero bits (unary code), return the count.
    #[cfg(test)]
    fn read_unary(&mut self) -> Option<u32> {
        let mut count = 0u32;
        loop {
            let bit = self.read_bit()?;
            if bit != 0 {
                return Some(count);
            }
            count += 1;
            // Safety limit to prevent infinite loop on corrupt data
            if count > 65536 {
                return None;
            }
        }
    }
}

/// `read_code` de WavPack : une valeur de 0 à `maxcode` inclus, en
/// `count_bits(maxcode)` bits ou un de moins.
fn read_code(bs: &mut BitstreamReader, maxcode: u32) -> u32 {
    if maxcode < 2 {
        return if maxcode != 0 { bs.getbit() } else { 0 };
    }

    let bitcount = count_bits(maxcode);
    let extras = ((1u64 << bitcount) - maxcode as u64 - 1) as u32;
    let mut code = bs.getbits(bitcount - 1);

    if code >= extras {
        code = (code << 1).wrapping_sub(extras).wrapping_add(bs.getbit());
    }

    code
}

/// Median tracking for adaptive entropy coding.
/// WavPack uses 3 medians per channel to adapt to signal statistics.
#[derive(Debug, Clone, Default)]
struct MedianValues {
    median: [u32; 3],
}

impl MedianValues {
    fn new() -> Self {
        Self::default()
    }

    /// `GET_MED(med)` : `(median[med] >> 4) + 1`.
    #[inline]
    fn get_med(&self, idx: usize) -> u32 {
        (self.median[idx] >> 4) + 1
    }

    /// Constantes de temps des trois médianes : `DIV0`, `DIV1`, `DIV2`.
    ///
    /// 🔴 Ce sont des CONSTANTES (128, 64, 32), pas `GET_MED(med)`. L'ancienne
    /// version divisait par la médiane elle-même : sur le premier échantillon
    /// réel mesuré, la médiane tombait à 852 au lieu de 870, et tout le flux
    /// entropique décalait derrière.
    const DIV: [u32; 3] = [128, 64, 32];

    /// `INC_MEDn()` : `median += ((median + DIV) / DIV) * 5`.
    #[inline]
    fn inc_med(&mut self, idx: usize) {
        let div = Self::DIV[idx];
        self.median[idx] = self.median[idx]
            .wrapping_add(((self.median[idx].wrapping_add(div)) / div).wrapping_mul(5));
    }

    /// `DEC_MEDn()` : `median -= ((median + DIV - 2) / DIV) * 2`.
    #[inline]
    fn dec_med(&mut self, idx: usize) {
        let div = Self::DIV[idx];
        self.median[idx] = self.median[idx]
            .wrapping_sub(((self.median[idx].wrapping_add(div - 2)) / div).wrapping_mul(2));
    }
}

/// État partagé du décodeur entropique (`struct words_data`).
///
/// 🔴 `holding_one`, `holding_zero` et `zeros_acc` sont communs AUX DEUX
/// canaux : le compte unaire d'un échantillon porte la moitié de celui du
/// suivant. C'est ce report qui manquait entièrement à l'ancien décodeur, et
/// sans lui le flux se désynchronise dès le deuxième échantillon.
struct WordsData {
    c: [MedianValues; 2],
    holding_one: u32,
    holding_zero: bool,
    zeros_acc: u32,
}

impl WordsData {
    fn new() -> Self {
        Self {
            c: [MedianValues::new(), MedianValues::new()],
            holding_one: 0,
            holding_zero: false,
            zeros_acc: 0,
        }
    }
}

/// Lit un compte de zéros / de uns codé « Elias gamma » (voir `read_words.c`).
/// Rend `None` sur fin de flux (`cbits == 33`).
fn read_gamma(bs: &mut BitstreamReader) -> Option<u32> {
    let mut cbits = 0u32;
    while cbits < 33 && bs.getbit() != 0 {
        cbits += 1;
    }

    if cbits == 33 {
        return None;
    }

    if cbits < 2 {
        return Some(cbits);
    }

    let mut mask = 1u32;
    let mut acc = 0u32;
    loop {
        cbits -= 1;
        if cbits == 0 {
            break;
        }
        if bs.getbit() != 0 {
            acc |= mask;
        }
        mask <<= 1;
    }

    Some(acc | mask)
}

/// `get_words_lossless` de WavPack : remplit `buffer` (entrelacé en stéréo)
/// avec les résidus entropiques. Rend le nombre d'échantillons produits.
fn get_words_lossless(
    w: &mut WordsData,
    bs: &mut BitstreamReader,
    buffer: &mut [i32],
    is_mono: bool,
) -> usize {
    let nsamples = buffer.len();
    let mut csamples = 0usize;

    while csamples < nsamples {
        let mut chan = if is_mono { 0 } else { csamples & 1 };

        if w.holding_zero {
            w.holding_zero = false;
            let maxcode = w.c[chan].get_med(0).wrapping_sub(1);
            let low = read_code(bs, maxcode);
            w.c[chan].dec_med(0);
            buffer[csamples] = if bs.getbit() != 0 {
                !(low as i32)
            } else {
                low as i32
            };

            csamples += 1;
            if csamples == nsamples {
                break;
            }
            chan = if is_mono { 0 } else { csamples & 1 };
        }

        if w.c[0].median[0] < 2 && w.holding_one == 0 && w.c[1].median[0] < 2 {
            if w.zeros_acc != 0 {
                w.zeros_acc -= 1;
                if w.zeros_acc != 0 {
                    buffer[csamples] = 0;
                    csamples += 1;
                    continue;
                }
            } else {
                match read_gamma(bs) {
                    None => break,
                    Some(v) => w.zeros_acc = v,
                }

                if w.zeros_acc != 0 {
                    w.c[0].median = [0; 3];
                    w.c[1].median = [0; 3];
                    buffer[csamples] = 0;
                    csamples += 1;
                    continue;
                }
            }
        }

        // Compte unaire, plafonné à LIMIT_ONES puis étendu en Elias gamma.
        let mut ones_count = 0u32;
        while ones_count < LIMIT_ONES + 1 && bs.getbit() != 0 {
            ones_count += 1;
        }

        if ones_count >= LIMIT_ONES {
            if ones_count == LIMIT_ONES + 1 {
                break; // WORD_EOF
            }
            match read_gamma(bs) {
                None => break,
                Some(v) => ones_count = v + LIMIT_ONES,
            }
        }

        // Report d'un demi-compte sur l'échantillon suivant.
        let carry = w.holding_one;
        w.holding_one = ones_count & 1;
        w.holding_zero = (!ones_count) & 1 != 0;
        let ones_count = (ones_count >> 1) + carry;

        let c = &mut w.c[chan];
        let mut low: u32;
        let high: u32;

        // Arithmetique NON SIGNEE et modulaire, comme en C : un fichier abime
        // peut faire deborder ces sommes, et une panique dans un decodeur est
        // un deni de service, pas une protection. Le CRC du bloc refusera le
        // resultat de toute facon.
        if ones_count == 0 {
            low = 0;
            high = c.get_med(0) - 1;
            c.dec_med(0);
        } else {
            low = c.get_med(0);
            c.inc_med(0);

            if ones_count == 1 {
                high = low.wrapping_add(c.get_med(1)).wrapping_sub(1);
                c.dec_med(1);
            } else {
                low = low.wrapping_add(c.get_med(1));
                c.inc_med(1);

                if ones_count == 2 {
                    high = low.wrapping_add(c.get_med(2)).wrapping_sub(1);
                    c.dec_med(2);
                } else {
                    low = low.wrapping_add((ones_count - 2).wrapping_mul(c.get_med(2)));
                    high = low.wrapping_add(c.get_med(2)).wrapping_sub(1);
                    c.inc_med(2);
                }
            }
        }

        low = low.wrapping_add(read_code(bs, high.wrapping_sub(low)));
        buffer[csamples] = if bs.getbit() != 0 {
            !(low as i32)
        } else {
            low as i32
        };
        csamples += 1;
    }

    csamples
}

// ── Decorrelation ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DecorrPass {
    term: i32,
    delta: i32,
    weight_a: i32,
    weight_b: i32,
    samples_a: [i32; 8],
    samples_b: [i32; 8],
}

impl DecorrPass {
    fn new() -> Self {
        Self {
            term: 0,
            delta: 0,
            weight_a: 0,
            weight_b: 0,
            samples_a: [0; 8],
            samples_b: [0; 8],
        }
    }
}

fn parse_decorr_terms(data: &[u8]) -> Vec<DecorrPass> {
    // Each byte encodes term and delta: value = ((term + 5) | (delta << 5)) & 0xFF
    // term = (byte & 0x1F) - 5
    // delta = (byte >> 5) & 0x07
    let mut passes = Vec::new();
    for &b in data.iter().rev() {
        // terms are stored in reverse order
        let term = (b & 0x1F) as i32 - 5;
        let delta = ((b >> 5) & 0x07) as i32;
        let mut pass = DecorrPass::new();
        pass.term = term;
        pass.delta = delta;
        passes.push(pass);
    }
    passes
}

/// `read_decorr_weights`. 🔴 Les poids sont assignés **de la DERNIÈRE passe
/// vers la première** (`while (--dpp >= wps->decorr_passes)`), et un bloc peut
/// en porter moins qu'il n'y a de termes — les passes de tête restent alors à
/// zéro. L'ancienne version remplissait depuis la passe 0, donc à l'envers.
fn parse_decorr_weights(data: &[u8], passes: &mut [DecorrPass], is_mono: bool) {
    for pass in passes.iter_mut() {
        pass.weight_a = 0;
        pass.weight_b = 0;
    }

    let mut termcnt = if is_mono { data.len() } else { data.len() / 2 };
    let mut idx = 0usize;

    for pass in passes.iter_mut().rev() {
        if termcnt == 0 {
            break;
        }
        termcnt -= 1;

        if idx >= data.len() {
            break;
        }
        pass.weight_a = restore_weight(data[idx] as i8);
        idx += 1;

        if !is_mono {
            if idx >= data.len() {
                break;
            }
            pass.weight_b = restore_weight(data[idx] as i8);
            idx += 1;
        }
    }
}

/// `restore_weight` de WavPack :
/// ```c
/// if ((result = (int) weight * 8) > 0)
///     result += (result + 64) >> 7;
/// ```
/// 🔴 La correction d'arrondi ne s'applique QU'AUX poids positifs. L'ancienne
/// version la retranchait aussi des poids négatifs : jusqu'à 7 rangs d'écart
/// sur un poids, soit une prédiction fausse à chaque échantillon.
fn restore_weight(stored: i8) -> i32 {
    let mut result = (stored as i32) * 8;
    if result > 0 {
        result += (result + 64) >> 7;
    }
    result
}

/// `read_decorr_samples`. 🔴 Comme les poids, les échantillons sont lus **de la
/// dernière passe vers la première**, et chaque valeur est un `int16_t` passé
/// par `exp2s` — d'où l'importance du signe corrigé dans `exp2s`.
fn parse_decorr_samples(data: &[u8], passes: &mut [DecorrPass], is_mono: bool) {
    for pass in passes.iter_mut() {
        pass.samples_a = [0; 8];
        pass.samples_b = [0; 8];
    }

    let mut offset = 0usize;
    let take = |data: &[u8], offset: &mut usize| -> Option<i32> {
        if *offset + 2 > data.len() {
            return None;
        }
        let v = i16::from_le_bytes([data[*offset], data[*offset + 1]]) as i32;
        *offset += 2;
        Some(exp2s(v))
    };

    for pass in passes.iter_mut().rev() {
        if offset >= data.len() {
            break;
        }

        if pass.term > MAX_TERM {
            // Termes 17 et 18 : deux échantillons par canal.
            for j in 0..2 {
                match take(data, &mut offset) {
                    Some(v) => pass.samples_a[j] = v,
                    None => return,
                }
            }
            if !is_mono {
                for j in 0..2 {
                    match take(data, &mut offset) {
                        Some(v) => pass.samples_b[j] = v,
                        None => return,
                    }
                }
            }
        } else if pass.term < 0 {
            // Termes croisés : un échantillon par canal, A puis B.
            match take(data, &mut offset) {
                Some(v) => pass.samples_a[0] = v,
                None => return,
            }
            match take(data, &mut offset) {
                Some(v) => pass.samples_b[0] = v,
                None => return,
            }
        } else {
            // Termes 1-8 : `term` échantillons par canal, entrelacés A/B.
            for m in 0..pass.term.max(0) as usize {
                match take(data, &mut offset) {
                    Some(v) => pass.samples_a[m] = v,
                    None => return,
                }
                if !is_mono {
                    match take(data, &mut offset) {
                        Some(v) => pass.samples_b[m] = v,
                        None => return,
                    }
                }
            }
        }
    }
}

/// `apply_weight_i` : variante sans débordement possible sur 32 bits.
#[inline]
fn apply_weight_i(weight: i32, sample: i32) -> i32 {
    (weight.wrapping_mul(sample).wrapping_add(512)) >> 10
}

/// `apply_weight_f` : variante utilisée quand l'échantillon déborde de 16 bits.
/// ⚠️ Ce n'est PAS `(w * s + 512) >> 10` : l'arrondi diffère, et le format
/// dépend de celui-ci au bit près.
#[inline]
fn apply_weight_f(weight: i32, sample: i32) -> i32 {
    ((((sample & 0xffff).wrapping_mul(weight)) >> 9)
        .wrapping_add(((sample & !0xffff_i32) >> 9).wrapping_mul(weight))
        .wrapping_add(1))
        >> 1
}

/// `apply_weight` : choisit la variante selon la magnitude de l'échantillon.
#[inline]
fn apply_weight(weight: i32, sample: i32) -> i32 {
    if sample != (sample as i16) as i32 {
        apply_weight_f(weight, sample)
    } else {
        apply_weight_i(weight, sample)
    }
}

/// `update_weight` : `weight += delta * sign(source ^ result)`.
#[inline]
fn update_weight(weight: &mut i32, delta: i32, source: i32, result: i32) {
    if source != 0 && result != 0 {
        let s = (source ^ result) >> 31;
        *weight = (delta ^ s).wrapping_add(weight.wrapping_sub(s));
    }
}

/// `update_weight_clip` : idem, borné à ±1024 (termes croisés uniquement).
#[inline]
fn update_weight_clip(weight: &mut i32, delta: i32, source: i32, result: i32) {
    if source != 0 && result != 0 {
        let s = (source ^ result) >> 31;
        let mut w = (*weight ^ s).wrapping_add(delta.wrapping_sub(s));
        if w > 1024 {
            w = 1024;
        }
        *weight = (w ^ s).wrapping_sub(s);
    }
}

/// `decorr_stereo_pass` : une passe de décorrélation sur un tampon entrelacé.
fn decorr_stereo_pass(dpp: &mut DecorrPass, buffer: &mut [i32]) {
    let n = buffer.len() / 2;
    let delta = dpp.delta;

    match dpp.term {
        17 => {
            for i in 0..n {
                let sam = dpp.samples_a[0]
                    .wrapping_mul(2)
                    .wrapping_sub(dpp.samples_a[1]);
                dpp.samples_a[1] = dpp.samples_a[0];
                let tmp = buffer[i * 2];
                dpp.samples_a[0] = apply_weight(dpp.weight_a, sam).wrapping_add(tmp);
                buffer[i * 2] = dpp.samples_a[0];
                update_weight(&mut dpp.weight_a, delta, sam, tmp);

                let sam = dpp.samples_b[0]
                    .wrapping_mul(2)
                    .wrapping_sub(dpp.samples_b[1]);
                dpp.samples_b[1] = dpp.samples_b[0];
                let tmp = buffer[i * 2 + 1];
                dpp.samples_b[0] = apply_weight(dpp.weight_b, sam).wrapping_add(tmp);
                buffer[i * 2 + 1] = dpp.samples_b[0];
                update_weight(&mut dpp.weight_b, delta, sam, tmp);
            }
        }
        18 => {
            for i in 0..n {
                let sam = dpp.samples_a[0]
                    .wrapping_add(dpp.samples_a[0].wrapping_sub(dpp.samples_a[1]) >> 1);
                dpp.samples_a[1] = dpp.samples_a[0];
                let tmp = buffer[i * 2];
                dpp.samples_a[0] = apply_weight(dpp.weight_a, sam).wrapping_add(tmp);
                buffer[i * 2] = dpp.samples_a[0];
                update_weight(&mut dpp.weight_a, delta, sam, tmp);

                let sam = dpp.samples_b[0]
                    .wrapping_add(dpp.samples_b[0].wrapping_sub(dpp.samples_b[1]) >> 1);
                dpp.samples_b[1] = dpp.samples_b[0];
                let tmp = buffer[i * 2 + 1];
                dpp.samples_b[0] = apply_weight(dpp.weight_b, sam).wrapping_add(tmp);
                buffer[i * 2 + 1] = dpp.samples_b[0];
                update_weight(&mut dpp.weight_b, delta, sam, tmp);
            }
        }
        -1 => {
            for i in 0..n {
                let sam = buffer[i * 2].wrapping_add(apply_weight(dpp.weight_a, dpp.samples_a[0]));
                update_weight_clip(&mut dpp.weight_a, delta, dpp.samples_a[0], buffer[i * 2]);
                buffer[i * 2] = sam;
                dpp.samples_a[0] = buffer[i * 2 + 1].wrapping_add(apply_weight(dpp.weight_b, sam));
                update_weight_clip(&mut dpp.weight_b, delta, sam, buffer[i * 2 + 1]);
                buffer[i * 2 + 1] = dpp.samples_a[0];
            }
        }
        -2 => {
            for i in 0..n {
                let sam =
                    buffer[i * 2 + 1].wrapping_add(apply_weight(dpp.weight_b, dpp.samples_b[0]));
                update_weight_clip(
                    &mut dpp.weight_b,
                    delta,
                    dpp.samples_b[0],
                    buffer[i * 2 + 1],
                );
                buffer[i * 2 + 1] = sam;
                dpp.samples_b[0] = buffer[i * 2].wrapping_add(apply_weight(dpp.weight_a, sam));
                update_weight_clip(&mut dpp.weight_a, delta, sam, buffer[i * 2]);
                buffer[i * 2] = dpp.samples_b[0];
            }
        }
        -3 => {
            for i in 0..n {
                let sam_a =
                    buffer[i * 2].wrapping_add(apply_weight(dpp.weight_a, dpp.samples_a[0]));
                update_weight_clip(&mut dpp.weight_a, delta, dpp.samples_a[0], buffer[i * 2]);
                let sam_b =
                    buffer[i * 2 + 1].wrapping_add(apply_weight(dpp.weight_b, dpp.samples_b[0]));
                update_weight_clip(
                    &mut dpp.weight_b,
                    delta,
                    dpp.samples_b[0],
                    buffer[i * 2 + 1],
                );
                dpp.samples_b[0] = sam_a;
                buffer[i * 2] = sam_a;
                dpp.samples_a[0] = sam_b;
                buffer[i * 2 + 1] = sam_b;
            }
        }
        _ => {
            // Termes 1 à 8 : tampon circulaire de 8 échantillons par canal.
            let mut m = 0usize;
            let mut k = (dpp.term & (MAX_TERM - 1)) as usize;
            for i in 0..n {
                let sam = dpp.samples_a[m];
                dpp.samples_a[k] = apply_weight(dpp.weight_a, sam).wrapping_add(buffer[i * 2]);
                update_weight(&mut dpp.weight_a, delta, sam, buffer[i * 2]);
                buffer[i * 2] = dpp.samples_a[k];

                let sam = dpp.samples_b[m];
                dpp.samples_b[k] = apply_weight(dpp.weight_b, sam).wrapping_add(buffer[i * 2 + 1]);
                update_weight(&mut dpp.weight_b, delta, sam, buffer[i * 2 + 1]);
                buffer[i * 2 + 1] = dpp.samples_b[k];

                m = (m + 1) & (MAX_TERM as usize - 1);
                k = (k + 1) & (MAX_TERM as usize - 1);
            }
        }
    }
}

/// `decorr_mono_pass`.
fn decorr_mono_pass(dpp: &mut DecorrPass, buffer: &mut [i32]) {
    let delta = dpp.delta;

    match dpp.term {
        17 => {
            for s in buffer.iter_mut() {
                let sam = dpp.samples_a[0]
                    .wrapping_mul(2)
                    .wrapping_sub(dpp.samples_a[1]);
                dpp.samples_a[1] = dpp.samples_a[0];
                dpp.samples_a[0] = apply_weight(dpp.weight_a, sam).wrapping_add(*s);
                update_weight(&mut dpp.weight_a, delta, sam, *s);
                *s = dpp.samples_a[0];
            }
        }
        18 => {
            for s in buffer.iter_mut() {
                let sam = dpp.samples_a[0]
                    .wrapping_mul(3)
                    .wrapping_sub(dpp.samples_a[1])
                    >> 1;
                dpp.samples_a[1] = dpp.samples_a[0];
                dpp.samples_a[0] = apply_weight(dpp.weight_a, sam).wrapping_add(*s);
                update_weight(&mut dpp.weight_a, delta, sam, *s);
                *s = dpp.samples_a[0];
            }
        }
        _ => {
            let mut m = 0usize;
            let mut k = (dpp.term & (MAX_TERM - 1)) as usize;
            for s in buffer.iter_mut() {
                let sam = dpp.samples_a[m];
                dpp.samples_a[k] = apply_weight(dpp.weight_a, sam).wrapping_add(*s);
                update_weight(&mut dpp.weight_a, delta, sam, *s);
                *s = dpp.samples_a[k];
                m = (m + 1) & (MAX_TERM as usize - 1);
                k = (k + 1) & (MAX_TERM as usize - 1);
            }

            if m != 0 {
                let tmp = dpp.samples_a;
                for (k, slot) in dpp.samples_a.iter_mut().enumerate() {
                    *slot = tmp[(m + k) & (MAX_TERM as usize - 1)];
                }
            }
        }
    }
}

// ── Block decoding ─────────────────────────────────────────────────────

/// Résultat du décodage d'un bloc : les deux canaux, plus le CRC calculé.
struct DecodedBlock {
    left: Vec<i32>,
    right: Vec<i32>,
}

/// Decode a single WavPack block into i32 samples (left and right channels).
///
/// 🔴 Rend `Err` dès que le CRC du bloc ne correspond pas. Sans ce contrôle,
/// une désynchronisation du flux binaire rendait des échantillons plein bande
/// SANS la moindre trace — c'est le mécanisme de #3849.
fn decode_block(header: &BlockHeader, block_data: &[u8]) -> Result<DecodedBlock, String> {
    let sub_blocks = parse_sub_blocks(block_data);

    let mut decorr_passes: Vec<DecorrPass> = Vec::new();
    let mut words = WordsData::new();
    let mut bitstream_data: &[u8] = &[];
    let mut wvx_data: Option<&[u8]> = None;
    let mut int32_info: Option<(u8, u8, u8, u8)> = None;

    let is_mono = header.is_mono_data();

    // 🔴 L'identifiant d'un sous-bloc tient sur SIX bits. Le bit 0x20
    // (`ID_OPTIONAL_DATA`) marque une métadonnée qu'un décodeur a le droit
    // d'ignorer. Masquer par 0x1F comme avant confond `0x25` (optionnel) avec
    // `0x05` (variables d'entropie) : sur un fichier produit par l'encodeur
    // officiel, les trois octets nuls de `0x25` écrasaient les six médianes,
    // et le flux se décodait à côté dès le premier échantillon.
    for sub in &sub_blocks {
        match sub.id {
            SUB_DECORR_TERMS => {
                decorr_passes = parse_decorr_terms(&sub.data);
            }
            SUB_DECORR_WEIGHTS => {
                parse_decorr_weights(&sub.data, &mut decorr_passes, is_mono);
            }
            SUB_DECORR_SAMPLES => {
                parse_decorr_samples(&sub.data, &mut decorr_passes, is_mono);
            }
            SUB_ENTROPY_VARS => {
                // 🔴 SIX valeurs de 16 bits, chacune passée par `exp2s` — pas
                // six mots de 32 bits lus tels quels (ancienne lecture).
                let need = if is_mono { 6 } else { 12 };
                if sub.data.len() < need {
                    return Err(format!(
                        "entropy vars too short: {} < {need}",
                        sub.data.len()
                    ));
                }
                for i in 0..3 {
                    let raw = u16::from_le_bytes([sub.data[i * 2], sub.data[i * 2 + 1]]) as i32;
                    words.c[0].median[i] = exp2s(raw) as u32;
                }
                if !is_mono {
                    for i in 0..3 {
                        let o = 6 + i * 2;
                        let raw = u16::from_le_bytes([sub.data[o], sub.data[o + 1]]) as i32;
                        words.c[1].median[i] = exp2s(raw) as u32;
                    }
                }
            }
            SUB_INT32_INFO => {
                if sub.data.len() >= 4 {
                    int32_info = Some((sub.data[0], sub.data[1], sub.data[2], sub.data[3]));
                }
            }
            id if id == SUB_BITSTREAM => {
                bitstream_data = &sub.data;
            }
            id if id == SUB_WVX_BITSTREAM => {
                // Les 4 premiers octets portent le CRC des bits supplémentaires.
                if sub.data.len() > 4 {
                    wvx_data = Some(&sub.data[4..]);
                }
            }
            _ => {}
        }
    }

    let num_samples = header.block_samples as usize;
    if num_samples == 0 {
        return Ok(DecodedBlock {
            left: Vec::new(),
            right: Vec::new(),
        });
    }

    let total = if is_mono {
        num_samples
    } else {
        num_samples * 2
    };
    let mut buffer = vec![0i32; total];

    let mut bs = BitstreamReader::new(bitstream_data);
    if !bitstream_data.is_empty() {
        let produced = get_words_lossless(&mut words, &mut bs, &mut buffer, is_mono);
        let frames = if is_mono { produced } else { produced / 2 };
        if frames != num_samples {
            return Err(format!(
                "bitstream ended early: {frames}/{num_samples} samples"
            ));
        }
    }

    // Passes de décorrélation, dans l'ordre de stockage.
    for dpp in decorr_passes.iter_mut() {
        if is_mono {
            decorr_mono_pass(dpp, &mut buffer);
        } else {
            decorr_stereo_pass(dpp, &mut buffer);
        }
    }

    // Joint stereo puis CRC — dans cet ordre, le CRC porte sur les valeurs
    // remixées. `bptr[0] += (bptr[1] -= (bptr[0] >> 1))`.
    let mut crc: u32 = 0xffff_ffff;
    if is_mono {
        for s in buffer.iter() {
            crc = crc.wrapping_mul(3).wrapping_add(*s as u32);
        }
    } else {
        let joint = header.is_joint_stereo();
        for i in 0..num_samples {
            if joint {
                let l = buffer[i * 2];
                let r = buffer[i * 2 + 1].wrapping_sub(l >> 1);
                buffer[i * 2 + 1] = r;
                buffer[i * 2] = l.wrapping_add(r);
            }
            let l = buffer[i * 2] as u32;
            let r = buffer[i * 2 + 1] as u32;
            crc = crc
                .wrapping_add(crc << 3)
                .wrapping_add(l << 1)
                .wrapping_add(l)
                .wrapping_add(r);
        }
    }

    if crc != header.crc {
        return Err(format!(
            "block CRC mismatch at index {}: computed {:08x}, expected {:08x}",
            header.block_index, crc, header.crc
        ));
    }

    if bs.overrun {
        return Err(format!("bitstream overrun at block {}", header.block_index));
    }

    // `fixup_samples` : bits supplémentaires, puis décalage final.
    let mut shift = header.left_shift();

    if let Some((sent_bits, zeros, ones, dups)) = int32_info {
        let (sent_bits, zeros, ones, dups) = (
            (sent_bits & 0x1f) as u32,
            (zeros & 0x1f) as u32,
            (ones & 0x1f) as u32,
            (dups & 0x1f) as u32,
        );

        if let Some(wvx) = wvx_data {
            // Les bits de poids faible voyagent dans un second flux (`WVX`).
            let mask = if sent_bits >= 32 {
                u32::MAX
            } else {
                (1u32 << sent_bits) - 1
            };
            let mut xbs = BitstreamReader::new(wvx);
            for s in buffer.iter_mut() {
                let data = xbs.getbits(sent_bits);
                *s = (((*s as u32) << sent_bits) | (data & mask)) as i32;
                apply_extended_int(s, zeros, ones, dups);
            }
            if xbs.overrun {
                return Err(format!(
                    "wvx bitstream overrun at block {}",
                    header.block_index
                ));
            }
        } else if sent_bits == 0 && (zeros + ones + dups) != 0 {
            for s in buffer.iter_mut() {
                apply_extended_int(s, zeros, ones, dups);
            }
        } else {
            // Rien à reconstituer échantillon par échantillon : tout se
            // ramène au décalage final.
            shift += zeros + sent_bits + ones + dups;
        }
    }

    shift &= 0x1f;
    if shift > 0 {
        for s in buffer.iter_mut() {
            *s = ((*s as u32) << shift) as i32;
        }
    }

    // Désentrelacement.
    let (left, right) = if is_mono {
        // `FALSE_STEREO` : bloc mono à restituer en stéréo identique.
        let right = if header.is_false_stereo() {
            buffer.clone()
        } else {
            Vec::new()
        };
        (buffer, right)
    } else {
        let mut left = Vec::with_capacity(num_samples);
        let mut right = Vec::with_capacity(num_samples);
        for i in 0..num_samples {
            left.push(buffer[i * 2]);
            right.push(buffer[i * 2 + 1]);
        }
        (left, right)
    };

    Ok(DecodedBlock { left, right })
}

/// Bits de poids faible reconstitués (`zeros` / `ones` / `dups`).
#[inline]
fn apply_extended_int(s: &mut i32, zeros: u32, ones: u32, dups: u32) {
    if zeros != 0 {
        *s = ((*s as u32) << zeros) as i32;
    } else if ones != 0 {
        *s = ((((*s).wrapping_add(1) as u32) << ones) as i32).wrapping_sub(1);
    } else if dups != 0 {
        let bit = *s & 1;
        *s = (((((*s).wrapping_add(bit)) as u32) << dups) as i32).wrapping_sub(bit);
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Parse a WavPack file and extract format information without decoding.
pub fn parse_wavpack(path: &str) -> Result<WavPackInfo, String> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut reader = BufReader::new(file);

    let header = read_block_header(&mut reader)?;

    if header.version < 0x0402 || header.version > 0x0410 {
        return Err(format!(
            "unsupported WavPack version: 0x{:04x}",
            header.version
        ));
    }

    if header.is_dsd() {
        return Err("DSD WavPack not supported".into());
    }

    if header.is_hybrid() {
        return Err("hybrid (lossy) WavPack not supported".into());
    }

    let mut sample_rate = header.sample_rate();
    let channels = if header.is_mono() || header.is_false_stereo() {
        if header.is_false_stereo() { 2 } else { 1 }
    } else {
        2
    };

    let bits_per_sample = header.bits_per_sample();

    // Check for non-standard sample rate in sub-blocks
    let data_size = header.block_size as usize - 24; // header fields after magic+size = 24 bytes
    let mut block_data = vec![0u8; data_size];
    reader
        .read_exact(&mut block_data)
        .map_err(|e| format!("read block data: {e}"))?;

    let sub_blocks = parse_sub_blocks(&block_data);
    for sub in &sub_blocks {
        // Check for sample rate sub-block (ID 0x27 = 0x07 with 0x20 "nondecoder" flag)
        if sub.id == SUB_SAMPLE_RATE && sub.data.len() >= 3 {
            sample_rate =
                sub.data[0] as u32 | (sub.data[1] as u32) << 8 | (sub.data[2] as u32) << 16;
        }
        // Check for channel info sub-block for multi-channel
        if sub.id & 0x1F == (SUB_CHANNEL_INFO & 0x1F) && sub.data.len() >= 2 {
            // channel_info stores actual channel count for > 2 channels
            // but we only support mono/stereo for now
        }
    }

    if sample_rate == 0 {
        sample_rate = 44100; // fallback
    }

    let total_samples = if header.total_samples == 0xFFFFFFFF {
        0 // unknown
    } else {
        header.total_samples as u64
    };

    Ok(WavPackInfo {
        channels,
        sample_rate,
        bits_per_sample,
        total_samples,
    })
}

/// Decode a WavPack file to interleaved i32 PCM.
///
/// 🔴 Un bloc dont le CRC ne colle pas fait ÉCHOUER le décodage. Rendre du
/// PCM faux plutôt qu'une erreur, c'est ce qui a envoyé du bruit blanc à
/// pleine échelle dans les DAC des testeurs pendant trois mois (#3849).
pub fn decode_wavpack_to_pcm(
    path: &str,
    _target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut reader = BufReader::new(file);

    // Read first header for format info
    let first_header = read_block_header(&mut reader)?;

    if first_header.version < 0x0402 || first_header.version > 0x0410 {
        return Err(format!(
            "unsupported WavPack version: 0x{:04x}",
            first_header.version
        ));
    }

    if first_header.is_dsd() {
        return Err("DSD WavPack not supported".into());
    }

    if first_header.is_hybrid() {
        return Err("hybrid (lossy) WavPack not supported".into());
    }

    let source_rate = {
        let r = first_header.sample_rate();
        if r == 0 { 44100 } else { r }
    };

    let source_channels = if first_header.is_mono() && !first_header.is_false_stereo() {
        1u32
    } else {
        2u32
    };

    let bits = first_header.bits_per_sample();

    // Seek back to start to process all blocks
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek: {e}"))?;

    let skip_samples = if seek_s > 0.0 {
        (seek_s * source_rate as f64) as u64
    } else {
        0
    };

    let max_samples = if max_duration_s > 0.0 {
        (max_duration_s * source_rate as f64 * source_channels as f64) as usize
    } else {
        usize::MAX
    };

    let mut all_samples: Vec<i32> = Vec::new();
    let mut samples_processed: u64 = 0;

    loop {
        if all_samples.len() >= max_samples {
            break;
        }

        // Try to read next block header
        let header = match read_block_header(&mut reader) {
            Ok(h) => h,
            Err(_) => break, // EOF or corrupt — done
        };

        // Read block data
        let data_size = if header.block_size >= 24 {
            header.block_size as usize - 24
        } else {
            break; // corrupt
        };

        let mut block_data = vec![0u8; data_size];
        if reader.read_exact(&mut block_data).is_err() {
            break; // EOF within block
        }

        if header.block_samples == 0 {
            continue; // metadata-only block
        }

        // Seek: skip blocks before the seek point
        let block_end = header.block_index as u64 + header.block_samples as u64;
        if block_end <= skip_samples {
            samples_processed = block_end;
            continue;
        }

        // Decode this block
        let decoded = match decode_block(&header, &block_data) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    error = %e,
                    block_index = header.block_index,
                    file = path,
                    "wavpack_block_decode_error"
                );
                return Err(format!("wavpack decode failed: {e}"));
            }
        };

        let is_mono = header.is_mono() && !header.is_false_stereo();
        let out_channels = if is_mono { 1 } else { 2 };

        // Collect i32 samples interleaved
        let start_in_block = if samples_processed < skip_samples {
            (skip_samples - samples_processed) as usize
        } else {
            0
        };

        for i in start_in_block..decoded.left.len() {
            if all_samples.len() >= max_samples {
                break;
            }
            all_samples.push(decoded.left[i]);
            if out_channels == 2 {
                let r = if i < decoded.right.len() {
                    decoded.right[i]
                } else {
                    decoded.left[i]
                };
                all_samples.push(r);
            }
        }

        samples_processed = block_end;
    }

    if all_samples.is_empty() {
        return Err("wavpack: no samples decoded".into());
    }

    let total_frames = all_samples.len() as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = path,
        samples = all_samples.len(),
        rate = source_rate,
        channels = source_channels,
        bits,
        duration_s,
        "decoded_wavpack_native"
    );

    Ok(DecodedAudio {
        samples_i32: all_samples,
        bit_depth: bits as u16,
        sample_rate: source_rate,
        channels: source_channels,
        duration_s,
        integrite: Default::default(),
    })
}
// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal WavPack block header as bytes.
    fn build_block_header(block_samples: u32, total_samples: u32, flags: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&WAVPACK_MAGIC);
        // block_size: header fields (24 bytes) + 0 data bytes
        buf.extend_from_slice(&24u32.to_le_bytes());
        // version
        buf.extend_from_slice(&0x0410u16.to_le_bytes());
        // track, index
        buf.push(0);
        buf.push(0);
        // total samples
        buf.extend_from_slice(&total_samples.to_le_bytes());
        // block index
        buf.extend_from_slice(&0u32.to_le_bytes());
        // block samples
        buf.extend_from_slice(&block_samples.to_le_bytes());
        // flags
        buf.extend_from_slice(&flags.to_le_bytes());
        // crc
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf
    }

    fn make_flags(bps_minus_1: u32, mono: bool, sr_index: u32) -> u32 {
        let mut flags = bps_minus_1 & 0x03;
        if mono {
            flags |= FLAG_MONO;
        }
        flags |= (sr_index & 0x0F) << 23;
        flags |= FLAG_INITIAL_BLOCK | FLAG_FINAL_BLOCK;
        flags
    }

    #[test]
    fn parse_header_stereo_16bit_44100() {
        let flags = make_flags(1, false, 9); // 2 bytes/sample, stereo, 44100
        let data = build_block_header(1024, 44100, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();

        assert_eq!(header.version, 0x0410);
        assert_eq!(header.bytes_per_sample(), 2);
        assert_eq!(header.bits_per_sample(), 16);
        assert!(!header.is_mono());
        assert_eq!(header.channels(), 2);
        assert_eq!(header.sample_rate(), 44100);
        assert_eq!(header.block_samples, 1024);
        assert_eq!(header.total_samples, 44100);
        assert!(!header.is_hybrid());
        assert!(!header.is_dsd());
    }

    #[test]
    fn parse_header_mono_24bit_96000() {
        let flags = make_flags(2, true, 13); // 3 bytes/sample, mono, 96000
        let data = build_block_header(512, 96000, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();

        assert_eq!(header.bits_per_sample(), 24);
        assert!(header.is_mono());
        assert_eq!(header.channels(), 1);
        assert_eq!(header.sample_rate(), 96000);
    }

    #[test]
    fn sample_rate_extraction_all() {
        for (idx, &expected) in SAMPLE_RATES.iter().enumerate() {
            if idx == 15 {
                continue; // 15 = unknown
            }
            let flags = make_flags(1, false, idx as u32);
            let data = build_block_header(0, 0, flags);
            let mut cursor = std::io::Cursor::new(&data);
            let header = read_block_header(&mut cursor).unwrap();
            assert_eq!(
                header.sample_rate(),
                expected,
                "sample rate index {idx} should be {expected}"
            );
        }
    }

    #[test]
    fn sample_rate_index_15_is_unknown() {
        let flags = make_flags(1, false, 15);
        let data = build_block_header(0, 0, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert_eq!(header.sample_rate(), 0);
    }

    #[test]
    fn bits_per_sample_all() {
        for bps_minus_1 in 0..=3 {
            let flags = make_flags(bps_minus_1, false, 9);
            let data = build_block_header(0, 0, flags);
            let mut cursor = std::io::Cursor::new(&data);
            let header = read_block_header(&mut cursor).unwrap();
            assert_eq!(
                header.bits_per_sample(),
                (bps_minus_1 + 1) * 8,
                "bps_minus_1={bps_minus_1}"
            );
        }
    }

    #[test]
    fn channel_detection_mono_vs_stereo() {
        let mono_flags = make_flags(1, true, 9);
        let stereo_flags = make_flags(1, false, 9);

        let mono_data = build_block_header(0, 0, mono_flags);
        let stereo_data = build_block_header(0, 0, stereo_flags);

        let mut c1 = std::io::Cursor::new(&mono_data);
        let h1 = read_block_header(&mut c1).unwrap();
        assert!(h1.is_mono());
        assert_eq!(h1.channels(), 1);

        let mut c2 = std::io::Cursor::new(&stereo_data);
        let h2 = read_block_header(&mut c2).unwrap();
        assert!(!h2.is_mono());
        assert_eq!(h2.channels(), 2);
    }

    #[test]
    fn false_stereo_flag() {
        let flags = make_flags(1, true, 9) | FLAG_FALSE_STEREO;
        let data = build_block_header(0, 0, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert!(header.is_false_stereo());
        assert!(header.is_mono()); // mono flag set
    }

    #[test]
    fn hybrid_flag_detection() {
        let flags = make_flags(1, false, 9) | FLAG_HYBRID;
        let data = build_block_header(0, 0, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert!(header.is_hybrid());
    }

    #[test]
    fn dsd_flag_detection() {
        let flags = make_flags(0, false, 9) | FLAG_DSD;
        let data = build_block_header(0, 0, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert!(header.is_dsd());
    }

    #[test]
    fn joint_stereo_flag() {
        let flags = make_flags(1, false, 9) | FLAG_JOINT_STEREO;
        let data = build_block_header(0, 0, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert!(header.is_joint_stereo());
    }

    #[test]
    fn left_shift_extraction() {
        let flags = make_flags(1, false, 9) | (2 << 13); // left_shift = 2
        let data = build_block_header(0, 0, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert_eq!(header.left_shift(), 2);
    }

    #[test]
    fn invalid_magic_rejected() {
        let mut data = build_block_header(0, 0, 0);
        data[0] = b'X'; // corrupt magic
        let mut cursor = std::io::Cursor::new(&data);
        let result = read_block_header(&mut cursor);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a WavPack block"));
    }

    #[test]
    fn total_samples_unknown() {
        let flags = make_flags(1, false, 9);
        let data = build_block_header(0, 0xFFFFFFFF, flags);
        let mut cursor = std::io::Cursor::new(&data);
        let header = read_block_header(&mut cursor).unwrap();
        assert_eq!(header.total_samples, 0xFFFFFFFF);
    }

    #[test]
    fn exp2s_signed_log() {
        // `wp_exp2s` prend un logarithme SIGNE : exp2s(-x) == -exp2s(x).
        assert_eq!(exp2s(0), 0);
        assert_eq!(exp2s(0x900), 256); // table[0]|0x100 = 256, exposant 9
        assert_eq!(exp2s(-0x900), -256);
        assert_eq!(exp2s(0xA00), 512); // exposant 10 : 256 << 1
        assert_eq!(exp2s(-0xA00), -512);
        // La mantisse passe par exp2_table, pas par l'octet brut :
        // 2^(0x80/256) = 1.414..., 256 * 1.414 = 362 -> table[0x80] = 0x6a.
        assert_eq!(exp2s(0x980), 0x16a);
    }

    /// Garde de non-regression sur #3849 : un log negatif de forte magnitude
    /// se decode en valeur negative bornee, PAS en decalage de plus de 100
    /// rangs. L'ancienne version prenait le bit 15 pour un signe et les bits
    /// 8-14 pour un exposant : `attempt to shift left with overflow` en
    /// debogage, echantillon arbitraire en production.
    #[test]
    fn exp2s_negative_log_is_not_a_huge_shift() {
        for raw in [0xFFFFu16, 0xF600, 0x8900, 0x8001] {
            let log = raw as i16 as i32;
            let v = exp2s(log);
            assert!(v <= 0, "log {log} doit rendre une valeur negative ou nulle");
            assert_eq!(v, -exp2s(-log), "exp2s doit etre impair");
        }
        // Borne exacte d'un `int16_t` : ne doit pas paniquer (l'ancienne
        // version y decalait de 118 rangs).
        let _ = exp2s(i16::MIN as i32);
    }

    #[test]
    fn restore_weight_values() {
        assert_eq!(restore_weight(0), 0);
        assert_eq!(restore_weight(1), 8 + 0); // (1<<3) + ((1+7)>>4) = 8 + 0 = 8
        assert_eq!(restore_weight(10), 80 + 1); // (10<<3) + ((10+7)>>4) = 80 + 1 = 81
        assert_eq!(restore_weight(-1), -8 + 0); // (-1<<3) - ((1+7)>>4) = -8 - 0 = -8
    }

    #[test]
    fn sub_block_parsing() {
        // Build a simple sub-block: ID=0x02 (decorr terms), size=1 word (2 bytes), data = [0x22, 0x00]
        let data = vec![
            0x02, // ID byte: not large, not odd, id=2
            0x01, // size = 1 word = 2 bytes
            0x22, 0x00, // data: term = (0x22 & 0x1F) - 5 = 34 - 5 = 29... wait
        ];
        let subs = parse_sub_blocks(&data);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, 0x02);
        assert_eq!(subs[0].data.len(), 2);
    }

    #[test]
    fn sub_block_odd_size() {
        // Odd-size sub-block: 3 bytes of actual data, padded to 4 (2 words)
        let data = vec![
            0x42, // ID byte: odd flag (0x40) set, id=2
            0x02, // size = 2 words = 4 bytes, but actual = 3
            0xAA, 0xBB, 0xCC, 0x00, // 3 bytes data + 1 padding
        ];
        let subs = parse_sub_blocks(&data);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, 0x02); // 0x42 & 0x3F = 0x02 (odd+large bits stripped)
        assert_eq!(subs[0].data.len(), 3); // odd size strips last byte
    }

    #[test]
    fn decorr_terms_parsing() {
        // term + 5 | (delta << 5): for term=1, delta=2: (1+5) | (2<<5) = 6 | 64 = 70
        let data = vec![70u8];
        let passes = parse_decorr_terms(&data);
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].term, 1);
        assert_eq!(passes[0].delta, 2);
    }

    #[test]
    fn decorr_terms_17_18() {
        // term 17: (17+5) | 0 = 22
        // term 18: (18+5) | 0 = 23
        let data = vec![22, 23]; // reversed: 23 first in stored order
        let passes = parse_decorr_terms(&data);
        assert_eq!(passes.len(), 2);
        // Reversed during parsing: first parsed = last in data
        assert_eq!(passes[0].term, 18);
        assert_eq!(passes[1].term, 17);
    }

    #[test]
    fn bitstream_reader_basics() {
        let data = [0b10110100u8, 0b01010011u8];
        let mut bs = BitstreamReader::new(&data);

        // LSB first: 0b10110100 -> bits: 0,0,1,0,1,1,0,1
        assert_eq!(bs.read_bit(), Some(0));
        assert_eq!(bs.read_bit(), Some(0));
        assert_eq!(bs.read_bit(), Some(1));
        assert_eq!(bs.read_bit(), Some(0));
        assert_eq!(bs.read_bit(), Some(1));
        assert_eq!(bs.read_bit(), Some(1));
        assert_eq!(bs.read_bit(), Some(0));
        assert_eq!(bs.read_bit(), Some(1));

        // Next byte: 0b01010011 -> bits: 1,1,0,0,1,0,1,0
        assert_eq!(bs.read_bit(), Some(1));
        assert_eq!(bs.read_bit(), Some(1));
    }

    #[test]
    fn bitstream_read_bits() {
        let data = [0xFF, 0x00];
        let mut bs = BitstreamReader::new(&data);
        // Read 4 bits from 0xFF (LSB first) = 0b1111 = 15
        assert_eq!(bs.read_bits(4), Some(0x0F));
        // Read next 4 bits from 0xFF = 0b1111 = 15
        assert_eq!(bs.read_bits(4), Some(0x0F));
        // Read 4 bits from 0x00 = 0
        assert_eq!(bs.read_bits(4), Some(0));
    }

    #[test]
    fn bitstream_read_unary() {
        // 0b00000101: LSB first = 1, 0, 1, 0, 0, 0, 0, 0
        let data = [0b00000101u8];
        let mut bs = BitstreamReader::new(&data);
        // First bit is 1, so unary = 0
        assert_eq!(bs.read_unary(), Some(0));
        // Next bit is 0, then 1: unary = 1
        assert_eq!(bs.read_unary(), Some(1));
    }

    #[test]
    fn median_values_get_inc_dec() {
        let mut m = MedianValues::new();
        assert_eq!(m.get_med(0), 1); // (0 >> 4) + 1 = 1

        m.median[0] = 160; // get_med(0) = (160 >> 4) + 1 = 11
        assert_eq!(m.get_med(0), 11);

        let before = m.median[0];
        m.inc_med(0);
        assert!(m.median[0] > before, "inc_med should increase median");

        let before = m.median[0];
        m.dec_med(0);
        assert!(m.median[0] < before, "dec_med should decrease median");
    }

    #[test]
    fn apply_weight_calculation() {
        assert_eq!(apply_weight(1024, 1000), 1000); // (1024 * 1000 + 512) >> 10 = 1000
        assert_eq!(apply_weight(0, 1000), 0);
        assert_eq!(apply_weight(512, 1000), 500); // (512 * 1000 + 512) >> 10 ≈ 500
    }

    #[test]
    fn parse_nonexistent_file() {
        let result = parse_wavpack("/nonexistent/file.wv");
        assert!(result.is_err());
    }

    // ── Fichiers WavPack REELS (#3849) ─────────────────────────────────
    //
    // Depuis la PR #55, ce decodeur n'avait jamais vu un `.wv` produit par
    // l'encodeur officiel : la case « Test with real .wv files (lossless) »
    // du plan de test n'a jamais ete cochee, et les 30 tests unitaires
    // portaient sur des en-tetes fabriques a la main. Resultat mesure le
    // 11/09/2026 sur un fichier de l'encodeur WavPack 5.6.0 : 264 569
    // echantillons faux sur 264 600, SNR -109 dB — du bruit a pleine bande,
    // exactement ce que Marco Polo entendait sur ses 13 albums.
    //
    // Les quatre fichiers de `tests/fixtures/wavpack/` viennent de
    // `wavpack 5.6.0` (Ubuntu noble, paquet officiel). Les empreintes
    // attendues sont celles de `wvunpack` 5.6.0 — le decodeur de REFERENCE —
    // et non celles de ce module : un temoin qui se nourrit de sa propre
    // sortie ne garde rien.
    //
    // Fabrication reproductible (Linux, paquet `wavpack`) :
    //   wavpack -y -q             source_16_44100_stereo.wav -o rip_16_44100_stereo.wv
    //   wavpack -y -q -hh -x4     source_24_96000_stereo.wav -o hires_24_96000_stereo.wv
    //   wavpack -y -q             source_16_44100_mono.wav   -o mono_16_44100.wv
    //   wavpack -y -q             source_24_44100_shift8.wav -o falsestereo_shift8_24_44100.wv
    //   wvunpack -y -q <f>.wv -o ref.wav      # puis empreinte des i32 en LE

    fn fixture_path(name: &str) -> String {
        format!(
            "{}/tests/fixtures/wavpack/{name}",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    fn empreinte_i32(samples: &[i32]) -> String {
        use md5::{Digest, Md5};
        let mut h = Md5::new();
        for s in samples {
            h.update(s.to_le_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// (fichier, canaux, cadence, profondeur, nb d'echantillons, empreinte)
    const FIXTURES: &[(&str, u32, u32, u16, usize, &str)] = &[
        // Rip CD : 16 bits / 44,1 kHz stereo, joint stereo — le cas de #3849.
        (
            "rip_16_44100_stereo.wv",
            2,
            44100,
            16,
            17640,
            "b0bf58385502cddf726dd94e6a542ae0",
        ),
        // Haute resolution 24/96 en mode `-hh -x4` : termes de decorrelation
        // supplementaires, et magnitudes qui basculent `apply_weight` sur la
        // variante « f » (arrondi different, et le format en depend).
        (
            "hires_24_96000_stereo.wv",
            2,
            96000,
            24,
            11520,
            "feb8d2d3a0f4763167bc6f3032ea82b0",
        ),
        // Mono : `MONO_FLAG`, six variables d'entropie au lieu de douze.
        (
            "mono_16_44100.wv",
            1,
            44100,
            16,
            8820,
            "4952d667c890843d942caf56e0b653b0",
        ),
        // `FALSE_STEREO` (deux canaux identiques, encodes en un seul) et
        // `INT32_DATA` avec `zeros = 8`. Le drapeau FALSE_STEREO etait lu au
        // bit 27 au lieu du bit 30 : le bloc passait pour du vrai stereo, et
        // les six variables d'entropie qu'il porte etaient lues comme douze.
        (
            "falsestereo_int32zeros_24_44100.wv",
            2,
            44100,
            24,
            17640,
            "33fb92370bdebf5d15275b884e557378",
        ),
        // `SHIFT_MASK` = 8. ⚠️ Ce fichier a ete fabrique EXPRES pour ce
        // drapeau : un WAV `WAVE_FORMAT_EXTENSIBLE` de conteneur 24 bits avec
        // `wValidBitsPerSample = 16`, d'ou `shift = 24 - 16 = 8` dans
        // l'en-tete WavPack. La premiere version du jeu d'epreuves croyait le
        // couvrir et ne le couvrait PAS : l'encodeur avait choisi
        // `INT32_DATA/zeros` pour ce contenu, le decalage valait 0, et le
        // sabotage du masque restait VERT. Verifie a la main sur les
        // drapeaux : 0x14bd1832 >> 13 & 0x1F = 8, que le masque de deux bits
        // tronquait a 0 — soit 256 fois trop bas.
        (
            "shift8_24_44100_stereo.wv",
            2,
            44100,
            24,
            17640,
            "fde7ae70718c6bf728921f45133fc60c",
        ),
    ];

    #[test]
    fn decode_bit_a_bit_des_fichiers_wavpack_reels() {
        for (name, channels, rate, bits, count, md5) in FIXTURES {
            let path = fixture_path(name);
            assert!(
                std::path::Path::new(&path).exists(),
                "fixture absente : {path}"
            );

            let info = parse_wavpack(&path).unwrap_or_else(|e| panic!("{name}: parse: {e}"));
            assert_eq!(info.channels, *channels, "{name}: canaux (en-tete)");
            assert_eq!(info.sample_rate, *rate, "{name}: cadence (en-tete)");
            assert_eq!(info.bits_per_sample, *bits as u32, "{name}: profondeur");

            let audio = decode_wavpack_to_pcm(&path, None, None, 0.0, 0.0)
                .unwrap_or_else(|e| panic!("{name}: decodage refuse : {e}"));

            assert_eq!(audio.channels, *channels, "{name}: canaux (PCM)");
            assert_eq!(audio.sample_rate, *rate, "{name}: cadence (PCM)");
            assert_eq!(audio.bit_depth, *bits, "{name}: profondeur (PCM)");
            assert_eq!(
                audio.samples_i32.len(),
                *count,
                "{name}: nombre d'echantillons"
            );
            assert_eq!(
                empreinte_i32(&audio.samples_i32),
                *md5,
                "{name}: le PCM decode ne correspond pas a celui de wvunpack 5.6.0 — \
                 le decodeur n'est plus sans perte"
            );
        }
    }

    /// Le format porte un CRC par bloc. Tant qu'il n'etait pas verifie, une
    /// desynchronisation du flux binaire sortait en PCM plein bande sans une
    /// ligne de journal — c'est le mecanisme par lequel #3849 a pu durer trois
    /// mois. Ce temoin exige un refus dans les deux cas possibles.
    ///
    /// ⚠️ Le premier cas est le seul qui garde le CRC **sans ambiguite** : on
    /// n'abime pas le flux, on abime la SOMME stockee dans l'en-tete du bloc.
    /// Le nombre d'echantillons et la longueur du flux restent exacts, donc
    /// aucune des autres gardes (fin prematuree, depassement) ne peut se
    /// declencher — seule la comparaison de CRC peut refuser. Un temoin qui
    /// abimerait le flux pourrait rester VERT en ayant retire la verification
    /// de CRC, et garderait alors autre chose que ce qu'il annonce.
    #[test]
    fn un_bloc_abime_est_refuse_et_non_rendu_en_bruit() {
        let src = std::fs::read(fixture_path("rip_16_44100_stereo.wv")).unwrap();
        let dir = crate::test_scratch::scratch_dir("wavpack-crc");

        let decoder = |octets: &[u8], nom: &str| -> Result<DecodedAudio, String> {
            let chemin = dir.join(nom);
            std::fs::write(&chemin, octets).unwrap();
            decode_wavpack_to_pcm(chemin.to_str().unwrap(), None, None, 0.0, 0.0)
        };

        // 1. La SOMME du premier bloc est fausse d'un bit. Le flux, lui, est
        //    intact : seule la verification de CRC peut s'en apercevoir.
        let mut somme_fausse = src.clone();
        somme_fausse[28] ^= 0x01; // champ `crc` de l'en-tete de bloc (offset 28..32)
        let err = decoder(&somme_fausse, "somme.wv").err().unwrap_or_else(|| {
            panic!(
                "un bloc dont le CRC ne correspond pas a ete decode SANS erreur : \
                 la verification de CRC n'est plus faite, et une desynchronisation \
                 repartira en bruit blanc silencieux (#3849)"
            )
        });
        assert!(
            err.contains("CRC"),
            "le refus doit nommer le CRC, obtenu : {err}"
        );

        // 2. Le flux entropique lui-meme est abime. Peu importe laquelle des
        //    gardes se declenche : ce qui ne doit PAS arriver, c'est du PCM.
        let mut flux_abime = src.clone();
        flux_abime[1024] ^= 0xFF;
        let err = decoder(&flux_abime, "flux.wv").err().unwrap_or_else(|| {
            panic!(
                "un flux WavPack abime a ete decode SANS erreur : c'est du bruit \
                 servi au DAC, pas de la musique"
            )
        });
        assert!(
            err.contains("CRC") || err.contains("bitstream"),
            "le refus doit nommer la cause, obtenu : {err}"
        );
    }
}
