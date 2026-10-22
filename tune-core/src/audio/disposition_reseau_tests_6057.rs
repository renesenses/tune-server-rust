//! #6057 — le chemin RÉSEAU route par la disposition que le fichier déclare
//! quand il change le nombre de canaux (renderer qui en annonce moins, #4573).
//!
//! Le témoin : un WAV extensible 4 canaux déclaré `FL FR FC LFE` (masque
//! `0x0F`, la disposition du type 5 d'un DSF), dont seules la centrale et le
//! LFE portent du signal, replié en stéréo. Par la disposition déclarée, la
//! centrale arrive à parts égales sur les deux voies et le LFE n'est jamais
//! replié. Par l'ordre par défaut (`FL FR BL BR`), la centrale était lue comme
//! l'arrière gauche et le LFE comme l'arrière droit : gauche et droite
//! différaient.

/// WAV WAVE_FORMAT_EXTENSIBLE 16 bits, 4 canaux, masque `masque`, `trames`
/// trames de `[0, 0, centrale, lfe]`.
fn wav_4_canaux(masque: u32, trames: usize, centrale: i16, lfe: i16) -> Vec<u8> {
    let canaux = 4u16;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&canaux.to_le_bytes());
    fmt.extend_from_slice(&48_000u32.to_le_bytes());
    fmt.extend_from_slice(&(48_000u32 * 2 * u32::from(canaux)).to_le_bytes());
    fmt.extend_from_slice(&(2 * canaux).to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&masque.to_le_bytes());
    fmt.extend_from_slice(&[
        0x01, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
    ]);
    let mut data = Vec::with_capacity(trames * 8);
    for _ in 0..trames {
        for v in [0i16, 0, centrale, lfe] {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    v.extend_from_slice(&fmt);
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

/// `FL FR FC LFE`.
const MASQUE_FL_FR_FC_LFE: u32 = 0x0F;

fn fichier(octets: &[u8]) -> tempfile::NamedTempFile {
    let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    std::fs::write(f.path(), octets).unwrap();
    f
}

/// Gauche et droite d'un repli stéréo : la centrale à parts égales, le LFE
/// nulle part.
fn verifier_le_repli(stereo: &[i32]) {
    assert!(!stereo.is_empty(), "aucune trame décodée");
    for (t, trame) in stereo.chunks_exact(2).enumerate() {
        assert_eq!(
            trame[0], trame[1],
            "trame {t} : la centrale déclarée doit arriver à parts égales sur \
             gauche et droite, et le LFE nulle part — {trame:?} (routée comme \
             un FL FR BL BR par défaut ?)"
        );
        assert!(
            trame[0] > 0,
            "trame {t} : la centrale a disparu — {trame:?}"
        );
    }
}

#[test]
fn le_repli_reseau_d_un_fichier_entier_route_par_la_disposition_declaree_6057() {
    let f = fichier(&wav_4_canaux(MASQUE_FL_FR_FC_LFE, 64, 8_000, 4_000));
    let chemin = f.path().to_string_lossy().to_string();
    let decode = super::decode_to_pcm_routee(&chemin, None, Some(2), 0.0, 0.0).unwrap();
    assert_eq!(decode.channels, 2);
    verifier_le_repli(&decode.samples_i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_repli_reseau_en_continu_route_par_la_disposition_declaree_6057() {
    let f = fichier(&wav_4_canaux(MASQUE_FL_FR_FC_LFE, 4_800, 8_000, 4_000));
    let chemin = f.path().to_string_lossy().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let decodeur = tokio::task::spawn_blocking(move || {
        super::decode_to_pcm_streaming(&chemin, None, Some(2), tx, 1_024)
    });
    let mut morceaux = Vec::new();
    while let Some(m) = rx.recv().await {
        morceaux.push(m);
    }
    let (bits, _) = decodeur.await.unwrap().unwrap();
    assert_eq!(bits, 16);
    let mut tout: Vec<u8> = morceaux.concat();
    if tout.starts_with(b"RIFF") {
        tout.drain(..44);
    }
    let stereo: Vec<i32> = tout
        .chunks_exact(2)
        .map(|o| i32::from(i16::from_le_bytes([o[0], o[1]])))
        .collect();
    verifier_le_repli(&stereo);
}

/// À nombre de canaux égal, le flux part tel quel : la sortie locale applique
/// elle-même la disposition déclarée, la réordonner ici la doublerait.
#[test]
fn a_nombre_de_canaux_egal_le_decodage_ne_reordonne_rien_6057() {
    let f = fichier(&wav_4_canaux(MASQUE_FL_FR_FC_LFE, 64, 8_000, 4_000));
    let chemin = f.path().to_string_lossy().to_string();
    let decode = super::decode_to_pcm_routee(&chemin, None, Some(4), 0.0, 0.0).unwrap();
    assert_eq!(decode.channels, 4);
    for trame in decode.samples_i32.chunks_exact(4) {
        assert_eq!(trame, [0, 0, 8_000, 4_000]);
    }
}

/// DSF de type 5 (`FL FR C LFE`), 4 canaux DSD64, `groupes` groupes de blocs :
/// avant et droite au motif de silence (`0x69`), centrale toute à 1, LFE tout
/// à 0 — deux continus de signes opposés, impossibles à confondre.
fn dsf_type_5(groupes: usize) -> Vec<u8> {
    const BLOC: usize = 4096;
    let canaux = 4u32;
    let mut donnees = Vec::with_capacity(groupes * BLOC * 4);
    for _ in 0..groupes {
        for octet in [0x69u8, 0x69, 0xFF, 0x00] {
            donnees.extend(std::iter::repeat_n(octet, BLOC));
        }
    }
    let total = 28 + 52 + 12 + donnees.len() as u64;
    let mut v = b"DSD ".to_vec();
    v.extend_from_slice(&28u64.to_le_bytes());
    v.extend_from_slice(&total.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes());
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&52u64.to_le_bytes());
    for x in [1u32, 0, 5, canaux, 2_822_400, 1] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&((groupes * BLOC * 8) as u64).to_le_bytes());
    v.extend_from_slice(&(BLOC as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(12 + donnees.len() as u64).to_le_bytes());
    v.extend_from_slice(&donnees);
    v
}

/// Le cas du type 5 d'un DSF servi en PCM à un renderer stéréo : la centrale
/// à parts égales, le LFE nulle part. Par l'ordre par défaut, la centrale
/// partait à gauche et le LFE (continu négatif) à droite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_repli_reseau_d_un_dsf_de_type_5_route_par_ses_positions_6057() {
    let f = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
    std::fs::write(f.path(), dsf_type_5(8)).unwrap();
    let chemin = f.path().to_string_lossy().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let decodeur = tokio::task::spawn_blocking(move || {
        super::decode_to_pcm_streaming(&chemin, None, Some(2), tx, 4_096)
    });
    let mut morceaux = Vec::new();
    while let Some(m) = rx.recv().await {
        morceaux.push(m);
    }
    let (bits, _) = decodeur.await.unwrap().unwrap();
    let mut tout: Vec<u8> = morceaux.concat();
    if tout.starts_with(b"RIFF") {
        tout.drain(..44);
    }
    let octets = usize::from(bits / 8);
    let mots: Vec<i32> = tout
        .chunks_exact(octets)
        .map(|o| {
            // Petit-boutiste, signe étendu depuis `bits`.
            let mut m = [0u8; 4];
            m[..octets].copy_from_slice(o);
            let decalage = u32::from(32 - bits);
            (i32::from_le_bytes(m) << decalage) >> decalage
        })
        .collect();
    assert!(!mots.is_empty(), "aucune trame décodée");
    let mut centrale_entendue = false;
    for (t, trame) in mots.chunks_exact(2).enumerate() {
        assert_eq!(
            trame[0], trame[1],
            "trame {t} : centrale à parts égales et LFE nulle part attendus — \
             {trame:?} (routé comme un FL FR BL BR par défaut ?)"
        );
        centrale_entendue |= trame[0] > 0;
    }
    assert!(
        centrale_entendue,
        "la centrale déclarée n'arrive nulle part"
    );
}
