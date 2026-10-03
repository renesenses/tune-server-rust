//! #5643, lot E — le flux « DSD brut » de la sortie ASIO native : des octets
//! d'un vrai fichier (DSF, DFF écrits sur disque) aux octets `DsdU8` que le
//! rappel ASIO recevra.
//!
//! L'attendu est calculé ICI, par un miroir de bits écrit à la main : une
//! garde qui appellerait `normaliser_en_dsd_u8` pour calculer son attendu ne
//! garderait rien.

use super::dsd_brut::*;

/// Un octet identifiable, jamais nul, qui dépend du canal ET de la position :
/// un échange de canaux ou un décalage d'un octet se voit.
fn octet_temoin(canal: usize, index: usize) -> u8 {
    (((canal * 97 + index * 31 + (index / 251) * 17) % 251) + 1) as u8
}

fn miroir(b: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        r |= ((b >> i) & 1) << (7 - i);
    }
    r
}

/// Un DSF : blocs PAR CANAL de `taille_bloc` octets, dernier bloc complété de
/// zéros. `bits_par_echantillon` = 1 (LSB-first, le cas courant) ou 8.
fn ecrire_dsf(
    chemin: &std::path::Path,
    canaux: u32,
    taille_bloc: u32,
    octets_par_canal: usize,
    bits_par_echantillon: u32,
) {
    let bloc = taille_bloc as usize;
    let blocs = octets_par_canal.div_ceil(bloc);
    let mut data = Vec::new();
    for b in 0..blocs {
        for canal in 0..canaux as usize {
            for i in 0..bloc {
                let idx = b * bloc + i;
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
    buf.extend_from_slice(&canaux.to_le_bytes());
    buf.extend_from_slice(&2_822_400u32.to_le_bytes());
    buf.extend_from_slice(&bits_par_echantillon.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&taille_bloc.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

/// Un DSDIFF non compressé : octets entrelacés, MSB-first.
fn ecrire_dff(chemin: &std::path::Path, canaux: u32, octets_par_canal: usize) {
    let mut data = Vec::new();
    for i in 0..octets_par_canal {
        for canal in 0..canaux as usize {
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
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&(2 + 4 * canaux as u64).to_be_bytes());
    prop.extend_from_slice(&(canaux as u16).to_be_bytes());
    for c in 0..canaux {
        prop.extend_from_slice(if c % 2 == 0 { b"SLFT" } else { b"SRGT" });
    }
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

/// Fait tourner le chemin de production et rend (blocs reçus, en-tête lu).
fn lire_tout(
    chemin: &std::path::Path,
    ext: &str,
    fenetre: Fenetre,
) -> (Vec<Vec<u8>>, EnteteDsdBrut) {
    let mut blocs = Vec::new();
    let e = lire_dsd_brut(chemin.to_str().unwrap(), ext, fenetre, |b| {
        blocs.push(b);
        Ok(())
    })
    .unwrap();
    (blocs, e)
}

/// Le témoin principal : un DSF stéréo (LSB-first, bloc de 4 096 octets,
/// longueur NON multiple du bloc pour éprouver le remplissage final) ressort
/// en `DsdU8` : un octet par canal et par trame, L puis R, chaque octet
/// miroité — et rien de plus (le remplissage de zéros n'est pas joué).
#[test]
fn un_dsf_ressort_en_dsd_u8_msb_first_entrelace_octet_par_octet() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("t.dsf");
    let octets_par_canal = 4096 * 2 + 1000;
    ecrire_dsf(&chemin, 2, 4096, octets_par_canal, 1);

    let (blocs, e) = lire_tout(&chemin, "dsf", Fenetre::default());
    assert_eq!(
        e,
        EnteteDsdBrut {
            cadence: 2_822_400,
            canaux: 2
        }
    );
    assert_eq!(
        lire_entete(&blocs[0]),
        Some(e),
        "le premier bloc est l'en-tête du flux"
    );
    let flux: Vec<u8> = blocs[1..].concat();
    assert_eq!(flux.len(), octets_par_canal * 2, "ni perte ni remplissage");
    for i in 0..octets_par_canal {
        for ch in 0..2 {
            assert_eq!(
                flux[i * 2 + ch],
                miroir(octet_temoin(ch, i)),
                "trame {i}, canal {ch} : DSF LSB-first non remis en MSB-first, ou canaux mal entrelacés"
            );
        }
    }
}

/// Un DFF est déjà MSB-first et entrelacé : il passe tel quel, sans miroir.
#[test]
fn un_dff_passe_tel_quel() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("t.dff");
    ecrire_dff(&chemin, 2, 5000);
    let (blocs, e) = lire_tout(&chemin, "dff", Fenetre::default());
    assert_eq!(e.canaux, 2);
    let flux: Vec<u8> = blocs[1..].concat();
    assert_eq!(flux.len(), 10_000);
    for i in 0..5000 {
        for ch in 0..2 {
            assert_eq!(
                flux[i * 2 + ch],
                octet_temoin(ch, i),
                "trame {i}, canal {ch}"
            );
        }
    }
}

/// Un DSF déclaré MSB-first (« bits per sample » = 8) n'est pas miroité.
#[test]
fn un_dsf_msb_first_n_est_pas_miroite() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("t.dsf");
    ecrire_dsf(&chemin, 2, 4096, 3000, 8);
    let (blocs, _) = lire_tout(&chemin, "dsf", Fenetre::default());
    let flux: Vec<u8> = blocs[1..].concat();
    assert_eq!(flux[0], octet_temoin(0, 0));
    assert_eq!(flux[1], octet_temoin(1, 0));
}

/// La fenêtre : une fin de 1 ms à DSD64 = 352,8 octets par canal → 352.
#[test]
fn la_fin_de_fenetre_borne_le_flux_dsf() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("t.dsf");
    ecrire_dsf(&chemin, 2, 4096, 20_000, 1);
    let (blocs, _) = lire_tout(
        &chemin,
        "dsf",
        Fenetre {
            debut_ms: 0,
            fin_ms: Some(1),
        },
    );
    let flux: Vec<u8> = blocs[1..].concat();
    assert_eq!(flux.len(), 352 * 2);
}

/// L'ordre des bits d'une source.
#[test]
fn l_ordre_des_bits_se_lit_sur_l_extension_et_le_champ_dsf() {
    assert!(source_lsb_first("dsf", Some(1)));
    assert!(source_lsb_first("DSF", None));
    assert!(!source_lsb_first("dsf", Some(8)));
    assert!(!source_lsb_first("dff", None));
    assert!(!source_lsb_first("iso", None));
}

/// L'en-tête : aller-retour, et refus de ce qui n'en est pas un (un WAV ne
/// doit jamais être pris pour du DSD brut).
#[test]
fn l_entete_fait_l_aller_retour_et_ne_reconnait_pas_un_wav() {
    let h = entete(5_644_800, 2);
    assert_eq!(
        lire_entete(&h),
        Some(EnteteDsdBrut {
            cadence: 5_644_800,
            canaux: 2
        })
    );
    let wav = crate::audio::wav::build_wav_header_with_duration(2, 44_100, 16, Some(1000));
    assert_eq!(lire_entete(&wav), None);
    assert_eq!(lire_entete(&h[..15]), None, "tronqué");
    assert_eq!(lire_entete(&entete(2_822_400, 0)), None, "zéro canal");
}

/// La durée d'un octet par canal : 8 bits à la cadence DSD.
#[test]
fn la_duree_se_compte_en_octets_par_canal() {
    let e = EnteteDsdBrut {
        cadence: 2_822_400,
        canaux: 2,
    };
    assert_eq!(e.ms_pour_octets_par_canal(352_800), 1000);
    assert_eq!(octets_par_canal_pour_ms(2_822_400, 1000), 352_800);
}

/// L'anneau : FIFO exacte à travers le bouclage, jamais de demi-trame.
#[test]
fn l_anneau_rend_les_octets_dans_l_ordre_et_par_trames_entieres() {
    let a = AnneauDsd::new(10);
    assert_eq!(
        a.pousser_trames(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], 2),
        10
    );
    let mut t = [0u8; 4];
    assert_eq!(a.tirer(&mut t), 4);
    assert_eq!(t, [1, 2, 3, 4]);
    // 4 octets libres, trames de 3 : une seule trame entre.
    assert_eq!(a.pousser_trames(&[20, 21, 22, 23, 24, 25], 3), 3);
    let mut reste = [0u8; 16];
    let n = a.tirer(&mut reste);
    assert_eq!(&reste[..n], &[5, 6, 7, 8, 9, 10, 20, 21, 22]);
}

/// Le rappel : intouché quand l'anneau suit, motif de repos 0x69 en pause et
/// en famine — jamais de zéros (un zéro DSD est un continu pleine échelle).
#[test]
fn le_rappel_ne_touche_pas_les_octets_et_comble_par_le_motif_de_repos() {
    let a = AnneauDsd::new(64);
    a.pousser(&[0xAA, 0x55, 0x0F, 0xF0]);
    let mut t = [0u8; 6];
    assert!(remplir_tampon_dsd(&a, false, &mut t), "famine signalée");
    assert_eq!(t, [0xAA, 0x55, 0x0F, 0xF0, 0x69, 0x69]);
    a.pousser(&[1, 2]);
    let mut p = [0u8; 2];
    assert!(!remplir_tampon_dsd(&a, true, &mut p));
    assert_eq!(p, [0x69, 0x69], "en pause : le motif de repos");
    assert_eq!(a.disponible(), 2, "la pause ne consomme rien");
}
