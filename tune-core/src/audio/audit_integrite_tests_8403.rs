//! #8403 — un FLAC intact suivi d'octets inconnus après sa dernière trame
//! (étiquette ID3v2 en fin de fichier, bourrage de zéros, données
//! quelconques : rien que `fin_des_donnees` sache retirer) était déclaré
//! « tronqué » ou « CRC-16 de la dernière trame incorrect ». Le décodage
//! complet le prouve sain par le MD5 de STREAMINFO ; une dernière trame
//! réellement abîmée ou coupée reste corrompue.
use super::*;
use md5::{Digest, Md5};
use std::io::Cursor;

/// 12 000 échantillons stéréo 16/44,1 : deux trames de 4 096 et une
/// dernière de 3 808.
fn flac() -> Vec<u8> {
    let pcm: Vec<u8> = (0..24000i32)
        .flat_map(|n| ((n * 97) as i16).to_le_bytes())
        .collect();
    let mut encodeur = super::super::encoder::AudioEncoder::new("flac", 44100, 16, 2);
    encodeur.start_sync().unwrap();
    encodeur.write_sync(&pcm).unwrap();
    let mut fichier = encodeur.finish_sync().unwrap();
    fichier[26..42].copy_from_slice(&Md5::digest(&pcm));
    fichier
}

/// Des queues que l'audit ne reconnaît pas comme étiquettes.
fn queues_inconnues() -> Vec<(&'static str, Vec<u8>)> {
    let mut id3v2 = b"ID3\x03\x00\x00\x00\x00\x07\x76".to_vec();
    id3v2.extend_from_slice(b"TIT2\x00\x00\x00\x08\x00\x00\x00Machine");
    id3v2.resize(1024, 0);
    vec![
        ("id3v2 en fin de fichier", id3v2),
        ("zéros", vec![0; 2000]),
        (
            "octets quelconques",
            (0..777u32)
                .map(|n| (n.wrapping_mul(2_654_435_761) >> 13) as u8)
                .collect(),
        ),
        ("un seul octet", vec![0x0a]),
    ]
}

fn auditer(octets: &[u8]) -> Resultat {
    verifier_source(
        Cursor::new(octets.to_vec()),
        octets.len() as u64,
        Some("flac"),
        || Ok(Cursor::new(octets.to_vec())),
        || true,
    )
    .unwrap()
}

#[test]
fn flac_intact_suivi_d_octets_inconnus_est_sain() {
    let f = flac();
    for (queue, octets) in queues_inconnues() {
        let fichier = [f.clone(), octets].concat();
        let r = auditer(&fichier);
        assert_eq!(
            (r.verdict.clone(), r.code.as_str(), r.trames_decodees),
            (Verdict::Sain, "sain", 12000),
            "FLAC intact suivi de « {queue} » déclaré corrompu : {r:?}"
        );
    }
}

#[test]
fn md5_nul_suivi_d_octets_inconnus_reste_non_verifiable() {
    let mut f = flac();
    f[26..42].fill(0);
    let fichier = [f, vec![0; 2000]].concat();
    let r = auditer(&fichier);
    assert_eq!(
        (r.verdict.clone(), r.code.as_str(), r.trames_decodees),
        (Verdict::NonVerifiable, "md5_nul", 12000),
        "{r:?}"
    );
}

#[test]
fn derniere_trame_abimee_suivie_d_octets_inconnus_reste_corrompue() {
    for (queue, octets) in queues_inconnues() {
        let mut f = flac();
        let n = f.len();
        // Un octet audio de la dernière trame change, CRC-16 compris : seul
        // le MD5 peut trancher, il ne doit pas être contourné.
        f[n - 10] ^= 0x55;
        let fichier = [f, octets].concat();
        let r = auditer(&fichier);
        assert_eq!(r.verdict, Verdict::Corrompu, "{queue} : {r:?}");
    }
}

#[test]
fn derniere_trame_coupee_suivie_d_octets_inconnus_reste_corrompue() {
    for (queue, octets) in queues_inconnues() {
        let f = flac();
        let n = f.len();
        let fichier = [f[..n - 200].to_vec(), octets].concat();
        let r = auditer(&fichier);
        assert_eq!(r.verdict, Verdict::Corrompu, "{queue} : {r:?}");
        assert!(r.trames_decodees < 12000, "{queue} : {r:?}");
    }
}
