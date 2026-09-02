//! Fabrique, DANS UN `TempDir`, un `.ape` multi-trames à partir du fixture
//! d'UNE SECONDE déjà versionné (`tests/fixtures/ape/sine_16s_c3000.ape`).
//!
//! Pourquoi fabriquer plutôt que versionner : #2505 parle de mémoire et de
//! délai avant le premier son. Éprouver l'un ou l'autre demande un fichier de
//! plusieurs TRAMES APE — une trame APE ≥ 3950 vaut 294 912 blocs, mais
//! l'encodeur du fixture n'en a écrit qu'une seule de 44 100 blocs. Poser un
//! `.ape` d'une heure dans le dépôt serait un blob de plusieurs dizaines de
//! mégaoctets versionné pour un seul test ; il est construit ici à l'exécution.
//!
//! Comment : les trames APE sont INDÉPENDANTES — `ApeDecoder::decode_frame`
//! remet à zéro prédicteurs et états entropiques au début de chaque trame, ce
//! dont `decode_all_parallel` (« Output is byte-identical to `decode_all()` »)
//! est la preuve dans la caisse elle-même. On peut donc répéter la trame codée
//! du fixture N fois et réécrire l'en-tête en conséquence : le PCM attendu est
//! exactement le WAV de référence répété N fois, ce qui donne au test un témoin
//! bit à bit et pas seulement une taille.

// Module partage par DEUX binaires de test : chacun n’en emploie qu’une
// partie, et `dead_code` se plaint de ce que l’autre emploie.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub struct MultiFrameApe {
    /// Handle du répertoire temporaire — GARDÉ EN VIE, sinon le fichier
    /// disparaît sous le décodeur.
    _dir: tempfile::TempDir,
    pub path: String,
    /// PCM natif attendu (16 bits stéréo petit-boutiste), sans en-tête WAV.
    pub expected_pcm: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
    /// Blocs par trame APE du fichier construit.
    pub blocks_per_frame: u32,
    pub total_frames: u32,
}

impl MultiFrameApe {
    /// Octets de PCM natif d'UNE trame APE : la granularité de décodage.
    pub fn frame_pcm_bytes(&self) -> usize {
        self.blocks_per_frame as usize * self.channels as usize * (self.bit_depth as usize / 8)
    }
}

/// Construit un `.ape` de `repeats` secondes (une trame APE par seconde).
pub fn build_multi_frame_ape(repeats: u32) -> MultiFrameApe {
    build(repeats, false)
}

/// Même fichier, DERNIÈRE TRAME SABOTÉE : son CRC ne tombe plus juste.
///
/// C'est la preuve déterministe que `max_duration_s` ARRÊTE le décodage au lieu
/// de tronquer après coup : un décodage borné avant la dernière trame réussit,
/// un décodage intégral échoue.
pub fn build_multi_frame_ape_corrupt_last(repeats: u32) -> MultiFrameApe {
    build(repeats, true)
}

fn build(repeats: u32, corrupt_last: bool) -> MultiFrameApe {
    assert!(repeats >= 2, "il faut au moins deux trames APE");
    let src = std::fs::read(fixtures_dir().join("ape/sine_16s_c3000.ape"))
        .expect("lire le fixture .ape du dépôt");
    let ref_wav = std::fs::read(fixtures_dir().join("ape/sine_16s_c3000.wav"))
        .expect("lire le WAV de référence du dépôt");

    // --- Descripteur (52 octets) ---
    assert_eq!(&src[0..4], b"MAC ", "descripteur APE attendu en tête");
    let descriptor_bytes = u32_at(&src, 8) as usize;
    let header_bytes = u32_at(&src, 12) as usize;
    let seek_table_bytes = u32_at(&src, 16) as usize;
    let header_data_bytes = u32_at(&src, 20) as usize;
    let frame_data_bytes = u32_at(&src, 24) as usize;

    // --- En-tête (24 octets) ---
    let h = descriptor_bytes;
    let final_frame_blocks = u32_at(&src, h + 8);
    let total_frames = u32_at(&src, h + 12);
    let bits_per_sample = u16::from_le_bytes([src[h + 16], src[h + 17]]);
    let channels = u16::from_le_bytes([src[h + 18], src[h + 19]]);
    let sample_rate = u32_at(&src, h + 20);
    assert_eq!(total_frames, 1, "le fixture du dépôt tient en une trame");

    let frames_off = descriptor_bytes + header_bytes + seek_table_bytes + header_data_bytes;
    let frame = &src[frames_off..frames_off + frame_data_bytes];
    // `seek_remainder(i) = (seek_byte(i) - seek_byte(0)) % 4` : en gardant une
    // longueur de trame multiple de 4, le reliquat reste 0 partout, comme dans
    // le fichier d'origine.
    assert_eq!(frame.len() % 4, 0, "longueur de trame alignée sur 4");

    let new_seek_table_bytes = 4 * repeats as usize;
    let new_frames_off = descriptor_bytes + header_bytes + new_seek_table_bytes + header_data_bytes;

    let mut out: Vec<u8> = Vec::with_capacity(new_frames_off + repeats as usize * frame.len());
    // Descripteur, corrigé.
    let mut descriptor = src[..descriptor_bytes].to_vec();
    put_u32(&mut descriptor, 16, new_seek_table_bytes as u32);
    put_u32(&mut descriptor, 24, (repeats as usize * frame.len()) as u32);
    put_u32(&mut descriptor, 28, 0); // frame_data_bytes_high
    put_u32(&mut descriptor, 32, 0); // terminating_data_bytes
    out.extend_from_slice(&descriptor);
    // En-tête, corrigé : chaque trame porte `final_frame_blocks` blocs, donc
    // `blocks_per_frame` doit valoir cette même valeur.
    let mut header = src[descriptor_bytes..descriptor_bytes + header_bytes].to_vec();
    put_u32(&mut header, 4, final_frame_blocks); // blocks_per_frame
    put_u32(&mut header, 8, final_frame_blocks); // final_frame_blocks
    put_u32(&mut header, 12, repeats); // total_frames
    out.extend_from_slice(&header);
    // Table de recherche : une entrée par trame.
    for i in 0..repeats as usize {
        out.extend_from_slice(&((new_frames_off + i * frame.len()) as u32).to_le_bytes());
    }
    // Données d'en-tête WAV d'origine, inchangées.
    out.extend_from_slice(
        &src[descriptor_bytes + header_bytes + seek_table_bytes
            ..descriptor_bytes + header_bytes + seek_table_bytes + header_data_bytes],
    );
    // Trames.
    for i in 0..repeats {
        if corrupt_last && i == repeats - 1 {
            let mut sabotee = frame.to_vec();
            // Au-delà des 4 octets de CRC, en plein milieu de la charge utile :
            // le PCM décodé change, le CRC stocké ne correspond plus.
            let milieu = sabotee.len() / 2;
            for b in sabotee[milieu..milieu + 64].iter_mut() {
                *b ^= 0x5a;
            }
            out.extend_from_slice(&sabotee);
        } else {
            out.extend_from_slice(frame);
        }
    }

    let dir = tempfile::TempDir::new().expect("TempDir");
    let path = dir.path().join(format!("sine_{repeats}s.ape"));
    std::fs::write(&path, &out).expect("écrire le .ape construit");

    let one_second_pcm = &ref_wav[44..]; // en-tête WAV canonique de 44 octets
    let mut expected_pcm = Vec::with_capacity(one_second_pcm.len() * repeats as usize);
    for _ in 0..repeats {
        expected_pcm.extend_from_slice(one_second_pcm);
    }

    MultiFrameApe {
        path: path.to_string_lossy().into_owned(),
        _dir: dir,
        expected_pcm,
        sample_rate,
        channels,
        bit_depth: bits_per_sample,
        blocks_per_frame: final_frame_blocks,
        total_frames: repeats,
    }
}
