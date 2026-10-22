//! #6456 (Levente, fil 2305) — un déplacement dans un `.dsf` servi en DoP sur
//! une sortie locale relançait la piste depuis le début : la branche DoP ne
//! transmettait la position qu'à une tranche d'image ISO. Le DoP d'un DSF ou
//! d'un DFF doit partir de la position demandée, et s'arrêter à la fin de la
//! tranche quand elle en a une.
//!
//! L'attendu est le DoP du fichier ENTIER, découpé ici à la main : la sortie
//! d'une tranche doit en être, octet pour octet, la portion correspondante.

use super::{decode_dsd_to_dop_streaming, decode_dsd_to_dop_streaming_tranche};
use tokio::sync::mpsc;

const CADENCE: u32 = 2_822_400;
const BLOC: usize = 4096;
/// Octets DSD par canal et par seconde en DSD64.
const OCTETS_PAR_SECONDE: usize = CADENCE as usize / 8;

fn octet_temoin(canal: usize, index: usize) -> u8 {
    (((canal * 97 + index * 31 + (index / 251) * 17) % 251) + 1) as u8
}

/// DSF stéréo, LSB-first, blocs de 4096 octets par canal.
fn ecrire_dsf(chemin: &std::path::Path, octets_par_canal: usize) {
    let blocs = octets_par_canal.div_ceil(BLOC);
    let mut data = Vec::new();
    for b in 0..blocs {
        for canal in 0..2 {
            for i in 0..BLOC {
                let idx = b * BLOC + i;
                data.push(if idx < octets_par_canal {
                    octet_temoin(canal, idx)
                } else {
                    0
                });
            }
        }
    }
    let total_samples = (octets_par_canal as u64) * 8;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&CADENCE.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&(BLOC as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

/// DSDIFF stéréo non compressé, octets entrelacés.
fn ecrire_dff(chemin: &std::path::Path, octets_par_canal: usize) {
    let mut data = Vec::new();
    for i in 0..octets_par_canal {
        for canal in 0..2 {
            data.push(octet_temoin(canal, i));
        }
    }
    let mut fver = Vec::new();
    fver.extend_from_slice(b"FVER");
    fver.extend_from_slice(&4u64.to_be_bytes());
    fver.extend_from_slice(&0x0105_0000u32.to_be_bytes());
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&CADENCE.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&(2 + 4 * 2u64).to_be_bytes());
    prop.extend_from_slice(&2u16.to_be_bytes());
    prop.extend_from_slice(b"SLFT");
    prop.extend_from_slice(b"SRGT");
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(b"DSD ");
    let frm8_size = 4 + fver.len() + 12 + prop.len() + 12 + data.len();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"FRM8");
    buf.extend_from_slice(&(frm8_size as u64).to_be_bytes());
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&fver);
    buf.extend_from_slice(b"PROP");
    buf.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    buf.extend_from_slice(&prop);
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&(data.len() as u64).to_be_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

fn lire_dop(chemin: &str, ext: &str, tranche: Option<(u64, Option<u64>)>) -> Vec<u8> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _garde = rt.enter();
    let (tx, mut rx) = mpsc::channel(4096);
    let mut premier = false;
    match tranche {
        Some((debut_ms, fin_ms)) => decode_dsd_to_dop_streaming_tranche(
            chemin,
            ext,
            debut_ms,
            fin_ms,
            tx,
            65536,
            &mut premier,
            &None,
            rt.handle(),
        ),
        None => {
            decode_dsd_to_dop_streaming(chemin, ext, tx, 65536, &mut premier, &None, rt.handle())
        }
    }
    .expect("DoP");
    let mut tout = Vec::new();
    while let Ok(bloc) = rx.try_recv() {
        tout.extend(bloc);
    }
    tout
}

/// Octets DoP (24 bits × 2 canaux) produits par `octets_par_canal` octets DSD
/// par canal : 2 octets DSD par canal et par trame DoP.
fn octets_dop(octets_par_canal: usize) -> usize {
    octets_par_canal / 2 * 6
}

/// Un saut à 1 s dans un fichier de 3 s : le DoP doit commencer à 1 s (au
/// bloc près, en deçà), et non au début du fichier.
#[test]
fn le_dop_d_un_dsf_et_d_un_dff_part_de_la_position_demandee() {
    let dir = tempfile::tempdir().unwrap();
    let octets_par_canal = 3 * OCTETS_PAR_SECONDE;
    let dsf = dir.path().join("piste.dsf");
    let dff = dir.path().join("piste.dff");
    ecrire_dsf(&dsf, octets_par_canal);
    ecrire_dff(&dff, octets_par_canal);

    // DSF : le déplacement retombe sur le bloc de 4096 octets qui le précède.
    let cible = OCTETS_PAR_SECONDE;
    for (chemin, ext, atteint) in [(&dsf, "dsf", cible / BLOC * BLOC), (&dff, "dff", cible)] {
        let chemin = chemin.to_str().unwrap();
        let entier = lire_dop(chemin, ext, None);
        assert_eq!(
            entier.len(),
            octets_dop(octets_par_canal),
            "{ext}: fichier entier"
        );
        let saut = lire_dop(chemin, ext, Some((1000, None)));
        let attendu = &entier[octets_dop(atteint)..];
        assert_eq!(
            saut.len(),
            attendu.len(),
            "{ext}: un saut à 1 s doit laisser {} octets DoP, pas le fichier entier ({})",
            attendu.len(),
            entier.len()
        );
        assert!(
            saut == attendu,
            "{ext}: le DoP après le saut doit être la suite du fichier"
        );
    }
}

/// Une tranche avec une fin (piste CUE dans un DSF/DFF) s'arrête à sa fin.
#[test]
fn la_tranche_d_un_dsf_et_d_un_dff_s_arrete_a_sa_fin() {
    let dir = tempfile::tempdir().unwrap();
    let octets_par_canal = 3 * OCTETS_PAR_SECONDE;
    let dsf = dir.path().join("image.dsf");
    let dff = dir.path().join("image.dff");
    ecrire_dsf(&dsf, octets_par_canal);
    ecrire_dff(&dff, octets_par_canal);
    let debut = OCTETS_PAR_SECONDE;
    let fin = 2 * OCTETS_PAR_SECONDE;
    for (chemin, ext, atteint) in [(&dsf, "dsf", debut / BLOC * BLOC), (&dff, "dff", debut)] {
        let chemin = chemin.to_str().unwrap();
        let entier = lire_dop(chemin, ext, None);
        let tranche = lire_dop(chemin, ext, Some((1000, Some(2000))));
        let attendu = &entier[octets_dop(atteint)..octets_dop(fin)];
        assert_eq!(
            tranche.len(),
            attendu.len(),
            "{ext}: longueur de la tranche 1 s..2 s"
        );
        assert!(tranche == attendu, "{ext}: contenu de la tranche 1 s..2 s");
    }
}
