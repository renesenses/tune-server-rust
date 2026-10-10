//! #6059 — un témoin par format, sur des fichiers synthétiques : le flux natif
//! servi à partir d'une position est un fichier VALIDE, commence à une
//! frontière du format juste avant la cible, et ses octets audio sont ceux du
//! fichier d'origine (bit-perfect).

use super::{DepartNatif, FormatNatif, preparer};
use std::path::{Path, PathBuf};

/// Le fichier virtuel que `serve_file` servira : en-tête puis tranche.
fn servi(chemin: &Path, d: &DepartNatif) -> Vec<u8> {
    let src = std::fs::read(chemin).unwrap();
    let debut = d.carte.body_src_start as usize;
    let fin = debut + d.carte.body_len as usize;
    let mut v = d.carte.header.clone();
    v.extend_from_slice(&src[debut..fin]);
    assert_eq!(
        v.len() as u64,
        d.carte.total,
        "taille annoncée = octets servis"
    );
    v
}

/// Un signal stéréo 16 bits sans motif répété : chaque trame est distincte,
/// un décalage d'une seule trame se voit.
fn pcm_s16_stereo(trames: usize) -> Vec<i16> {
    let mut v = Vec::with_capacity(trames * 2);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..trames * 2 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 16) as i16 / 4);
    }
    v
}

// ───────────────────────────── FLAC ─────────────────────────────

async fn flac_synthetique(dir: &Path, secondes: u32) -> PathBuf {
    let pcm = pcm_s16_stereo(44_100 * secondes as usize);
    let octets: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", 44_100, 16, 2);
    enc.start().await.unwrap();
    enc.write(&octets).await.unwrap();
    let chemin = dir.join("piste.flac");
    std::fs::write(&chemin, enc.finish().await.unwrap()).unwrap();
    chemin
}

fn decoder(chemin: &Path) -> Vec<i32> {
    crate::audio::decode::decode_to_pcm(&chemin.to_string_lossy(), None, None, 0.0, 0.0)
        .expect("décodable")
        .samples_i32
}

#[tokio::test]
async fn flac_part_d_une_trame_et_reste_bit_perfect_6059() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = flac_synthetique(dir.path(), 6).await;
    let d = preparer(&chemin, 3_200).expect("un FLAC se sert à partir d'une position");

    // Le flux servi commence par un en-tête FLAC, puis une trame.
    let v = servi(&chemin, &d);
    assert_eq!(&v[..4], b"fLaC");
    let premier_octet_audio = d.carte.header.len();
    assert_eq!(
        v[premier_octet_audio], 0xFF,
        "le corps commence par un code de synchro"
    );
    assert_eq!(v[premier_octet_audio + 1] & 0xFE, 0xF8);
    // Juste avant la cible, à moins d'une trame (4096 échantillons ≈ 93 ms).
    assert!(
        d.depart_ms <= 3_200,
        "départ {} après la cible",
        d.depart_ms
    );
    assert!(3_200 - d.depart_ms < 100, "départ {} trop tôt", d.depart_ms);

    // Bit-perfect : le flux servi se décode en EXACTEMENT la fin de l'original.
    let virtuel = dir.path().join("servi.flac");
    std::fs::write(&virtuel, &v).unwrap();
    let original = decoder(&chemin);
    let decoupe = decoder(&virtuel);
    assert!(!decoupe.is_empty());
    let decalage = original.len() - decoupe.len();
    assert_eq!(decalage % 2, 0);
    assert_eq!(
        (decalage / 2) as u64 * 1000 / 44_100,
        d.depart_ms,
        "le premier échantillon servi est celui annoncé"
    );
    assert!(
        original[decalage..] == decoupe[..],
        "les échantillons servis doivent être ceux du fichier, sans conversion"
    );
}

#[tokio::test]
async fn flac_reecrit_streaminfo_et_retire_la_seektable_6059() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = flac_synthetique(dir.path(), 4).await;
    let d = preparer(&chemin, 1_000).unwrap();
    let h = &d.carte.header;
    // Parcours des blocs de l'en-tête réécrit.
    let mut pos = 4;
    let mut types = Vec::new();
    loop {
        let dernier = h[pos] & 0x80 != 0;
        types.push(h[pos] & 0x7F);
        let len = u32::from_be_bytes([0, h[pos + 1], h[pos + 2], h[pos + 3]]) as usize;
        if types.len() == 1 {
            let si = &h[pos + 4..pos + 4 + len];
            assert!(
                si[18..34].iter().all(|b| *b == 0),
                "MD5 de la piste entière effacé"
            );
        }
        pos += 4 + len;
        if dernier {
            break;
        }
    }
    assert_eq!(
        pos,
        h.len(),
        "le drapeau « dernier bloc » est sur le dernier"
    );
    assert_eq!(types[0], 0, "STREAMINFO d'abord");
    assert!(
        !types.contains(&3),
        "une SEEKTABLE désignerait d'autres trames"
    );
}

// ───────────────────────────── AIFF ─────────────────────────────

fn etendu_80(v: u32) -> [u8; 10] {
    let mut expo = 16383 + 31;
    let mut m = v as u64;
    while m & 0x8000_0000 == 0 {
        m <<= 1;
        expo -= 1;
    }
    let mantisse = m << 32;
    let mut b = [0u8; 10];
    b[0..2].copy_from_slice(&(expo as u16).to_be_bytes());
    b[2..10].copy_from_slice(&mantisse.to_be_bytes());
    b
}

/// FORM/AIFF : COMM, un morceau de longueur IMPAIRE (bourrage), SSND avec un
/// offset non nul, puis un ID3 après le son.
fn aiff_synthetique(dir: &Path, trames: usize) -> (PathBuf, Vec<u8>) {
    let pcm: Vec<u8> = pcm_s16_stereo(trames)
        .iter()
        .flat_map(|s| s.to_be_bytes())
        .collect();
    let mut corps = Vec::new();
    corps.extend_from_slice(b"AIFF");
    corps.extend_from_slice(b"COMM");
    corps.extend_from_slice(&18u32.to_be_bytes());
    corps.extend_from_slice(&2u16.to_be_bytes());
    corps.extend_from_slice(&(trames as u32).to_be_bytes());
    corps.extend_from_slice(&16u16.to_be_bytes());
    corps.extend_from_slice(&etendu_80(44_100));
    corps.extend_from_slice(b"NAME");
    corps.extend_from_slice(&5u32.to_be_bytes());
    corps.extend_from_slice(b"Piste\0"); // 5 + bourrage
    corps.extend_from_slice(b"SSND");
    corps.extend_from_slice(&(8 + 4 + pcm.len() as u32).to_be_bytes());
    corps.extend_from_slice(&4u32.to_be_bytes()); // offset
    corps.extend_from_slice(&0u32.to_be_bytes());
    corps.extend_from_slice(&[0xAA; 4]);
    corps.extend_from_slice(&pcm);
    corps.extend_from_slice(b"ID3 ");
    corps.extend_from_slice(&4u32.to_be_bytes());
    corps.extend_from_slice(b"TAG!");
    let mut f = b"FORM".to_vec();
    f.extend_from_slice(&(corps.len() as u32).to_be_bytes());
    f.extend_from_slice(&corps);
    let chemin = dir.join("piste.aiff");
    std::fs::write(&chemin, &f).unwrap();
    (chemin, pcm)
}

#[test]
fn aiff_part_d_une_trame_de_ssnd_et_reste_bit_perfect_6059() {
    let dir = tempfile::tempdir().unwrap();
    let trames = 44_100 * 3;
    let (chemin, pcm) = aiff_synthetique(dir.path(), trames);
    let d = preparer(&chemin, 1_500).expect("un AIFF se sert à partir d'une position");
    let v = servi(&chemin, &d);

    let premiere = 44_100 * 1_500 / 1_000; // 66 150
    assert_eq!(d.depart_ms, 1_500);
    assert_eq!(&v[0..4], b"FORM");
    assert_eq!(
        u32::from_be_bytes(v[4..8].try_into().unwrap()) as usize,
        v.len() - 8,
        "taille FORM cohérente"
    );
    assert_eq!(&v[8..12], b"AIFF");
    // COMM : trames restantes.
    let comm = v.windows(4).position(|w| w == b"COMM").unwrap();
    let restantes = u32::from_be_bytes(v[comm + 10..comm + 14].try_into().unwrap());
    assert_eq!(restantes as usize, trames - premiere);
    // Le morceau impair est gardé, bourrage compris ; l'ID3 d'après le son, non.
    assert!(v.windows(5).any(|w| w == b"Piste"));
    // SSND : offset 0, puis EXACTEMENT les octets PCM du fichier depuis la trame.
    let ssnd = v.windows(4).position(|w| w == b"SSND").unwrap();
    let len = u32::from_be_bytes(v[ssnd + 4..ssnd + 8].try_into().unwrap()) as usize;
    assert_eq!(&v[ssnd + 8..ssnd + 16], &[0u8; 8]);
    let audio = &v[ssnd + 16..];
    assert_eq!(len, 8 + audio.len());
    assert!(
        audio == &pcm[premiere * 4..],
        "octets PCM du fichier, à la trame près"
    );

    // Et le décodeur AIFF de Tune le lit : exactement la fin de l'original.
    let virtuel = dir.path().join("servi.aiff");
    std::fs::write(&virtuel, &v).unwrap();
    let original = decoder(&chemin);
    let decoupe = decoder(&virtuel);
    assert_eq!(decoupe.len(), (trames - premiere) * 2);
    assert!(original[premiere * 2..] == decoupe[..]);
}

// ───────────────────────────── DSF ─────────────────────────────

/// DSF stéréo DSD64, blocs de 4096 octets par canal, `groupes` groupes.
fn dsf_synthetique(dir: &Path, groupes: usize) -> (PathBuf, Vec<u8>) {
    let donnees: Vec<u8> = (0..groupes * 2 * 4096)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3])
        .collect();
    let echantillons = (groupes * 4096 * 8) as u64;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + donnees.len() as u64 + 10).to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + donnees.len() as u64).to_le_bytes()); // ID3 à la fin
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&2_822_400u32.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&echantillons.to_le_bytes());
    buf.extend_from_slice(&4096u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + donnees.len() as u64).to_le_bytes());
    buf.extend_from_slice(&donnees);
    buf.extend_from_slice(b"ID3\x03\0\0\0\0\0\0");
    let chemin = dir.join("piste.dsf");
    std::fs::write(&chemin, &buf).unwrap();
    (chemin, donnees)
}

#[test]
fn dsf_part_d_un_groupe_de_blocs_et_reste_bit_perfect_6059() {
    let dir = tempfile::tempdir().unwrap();
    let (chemin, donnees) = dsf_synthetique(dir.path(), 4);
    // 25 ms = échantillon 70 560 → groupe 2 (65 536 échantillons par canal).
    let d = preparer(&chemin, 25).expect("un DSF se sert à partir d'une position");
    assert_eq!(d.depart_ms, 65_536 * 1000 / 2_822_400);
    let v = servi(&chemin, &d);
    let info = crate::audio::dsf::parse_dsf_from_bytes(&v).expect("un DSF valide");
    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_rate, 2_822_400);
    assert_eq!(info.total_samples, 4 * 32_768 - 2 * 32_768);
    assert_eq!(info.block_size, 4096);
    assert_eq!(info.data_size as usize, 2 * 2 * 4096);
    let audio = &v[info.data_offset as usize..];
    assert!(
        audio == &donnees[2 * 2 * 4096..],
        "groupes de blocs du fichier, sans conversion, alignés canal par canal"
    );
    assert_eq!(
        u64::from_le_bytes(v[20..28].try_into().unwrap()),
        0,
        "pas de pointeur de métadonnées : l'ID3 n'est pas servi"
    );
    assert_eq!(
        u64::from_le_bytes(v[12..20].try_into().unwrap()),
        v.len() as u64,
        "taille du fichier cohérente"
    );
}

// ───────────────────────────── bornes ─────────────────────────────

#[test]
fn hors_formats_hors_bornes_rien_6059() {
    let dir = tempfile::tempdir().unwrap();
    let (aiff, _) = aiff_synthetique(dir.path(), 44_100);
    assert!(
        preparer(&aiff, 0).is_none(),
        "position 0 : le fichier entier"
    );
    assert!(preparer(&aiff, 1_000).is_none(), "au-delà de la fin");
    let (dsf, _) = dsf_synthetique(dir.path(), 1);
    assert!(preparer(&dsf, 50).is_none(), "au-delà de la fin");
    assert_eq!(FormatNatif::du_chemin(Path::new("a.wav")), None);
    assert_eq!(FormatNatif::du_chemin(Path::new("a.dff")), None);
    assert_eq!(
        FormatNatif::du_chemin(Path::new("a.FLAC")),
        Some(FormatNatif::Flac)
    );
    let faux = dir.path().join("faux.flac");
    std::fs::write(&faux, b"pas un flac du tout").unwrap();
    assert!(preparer(&faux, 1_000).is_none());
}
