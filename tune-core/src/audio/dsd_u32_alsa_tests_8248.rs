//! #8248 — l'empaquetage `DsdU8` → `DSD_U32_BE`/`DSD_U32_LE` du DSD natif
//! ALSA, éprouvé au bit près depuis un VRAI fichier DSF écrit sur disque.
//!
//! L'attendu est calculé ICI, à la main, depuis les octets écrits dans le
//! fichier (miroir de bits du DSF LSB-first, puis quatre octets successifs
//! d'un même canal par mot) : une garde qui appellerait `empaqueter` pour
//! calculer son attendu ne garderait rien.

use super::dsd_brut::{Fenetre, SILENCE_DSD_MSB_FIRST, lire_dsd_brut, lire_entete};
use super::dsd_u32_alsa::*;

/// Un octet identifiable, jamais nul, qui dépend du canal ET de la position.
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

/// Un DSF stéréo LSB-first (bits per sample = 1), blocs par canal.
fn ecrire_dsf(chemin: &std::path::Path, canaux: u32, taille_bloc: u32, octets_par_canal: usize) {
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
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&taille_bloc.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

/// Le chemin de production complet : fichier DSF → `lire_dsd_brut` (flux
/// servi par l'orchestrateur, en-tête compris) → `Empaqueteur` alimenté par
/// morceaux de tailles irrégulières (comme les lectures HTTP).
fn flux_dsd_u32(chemin: &std::path::Path, ordre: OrdreDsdU32) -> Vec<u8> {
    let mut blocs = Vec::new();
    lire_dsd_brut(chemin.to_str().unwrap(), "dsf", Fenetre::default(), |b| {
        blocs.push(b);
        Ok(())
    })
    .unwrap();
    let entete = lire_entete(&blocs[0]).expect("en-tête du flux");
    let flux: Vec<u8> = blocs[1..].concat();
    let mut e = Empaqueteur::new(usize::from(entete.canaux), ordre);
    let mut sortie = Vec::new();
    // Lectures de 1, 7, 13, 1000… octets : jamais alignées sur un mot.
    let tailles = [1usize, 7, 13, 1000, 3, 4093];
    let mut pos = 0;
    let mut k = 0;
    while pos < flux.len() {
        let n = tailles[k % tailles.len()].min(flux.len() - pos);
        e.pousser(&flux[pos..pos + n], &mut sortie);
        pos += n;
        k += 1;
    }
    assert_eq!(
        e.en_attente(),
        0,
        "le témoin a un nombre d'octets multiple de 4"
    );
    sortie
}

/// Le témoin principal : `DSD_U32_BE` — par trame ALSA, un mot par canal (L
/// puis R) ; dans chaque mot, quatre octets SUCCESSIFS du canal, le plus
/// ancien en premier en mémoire, chacun remis en MSB-first depuis le DSF.
#[test]
fn un_dsf_ressort_en_dsd_u32_be_au_bit_pres() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("t.dsf");
    let octets_par_canal = 4096 * 2 + 1000; // multiple de 4, non multiple du bloc
    ecrire_dsf(&chemin, 2, 4096, octets_par_canal);

    let sortie = flux_dsd_u32(&chemin, OrdreDsdU32::GrosBoutiste);
    assert_eq!(
        sortie.len(),
        octets_par_canal * 2,
        "ni perte ni remplissage"
    );
    for trame in 0..octets_par_canal / 4 {
        for canal in 0..2 {
            for k in 0..4 {
                let attendu = miroir(octet_temoin(canal, trame * 4 + k));
                assert_eq!(
                    sortie[trame * 8 + canal * 4 + k],
                    attendu,
                    "trame ALSA {trame}, canal {canal}, octet {k} : DSD_U32_BE mal empaqueté \
                     (ordre du temps, canaux ou bits)"
                );
            }
        }
    }
}

/// `DSD_U32_LE` : le même mot, rangé petit-boutiste — l'octet le plus ancien
/// est le DERNIER des quatre en mémoire.
#[test]
fn un_dsf_ressort_en_dsd_u32_le_au_bit_pres() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("t.dsf");
    let octets_par_canal = 4096 + 512;
    ecrire_dsf(&chemin, 2, 4096, octets_par_canal);

    let sortie = flux_dsd_u32(&chemin, OrdreDsdU32::PetitBoutiste);
    assert_eq!(sortie.len(), octets_par_canal * 2);
    for trame in 0..octets_par_canal / 4 {
        for canal in 0..2 {
            for k in 0..4 {
                let attendu = miroir(octet_temoin(canal, trame * 4 + (3 - k)));
                assert_eq!(
                    sortie[trame * 8 + canal * 4 + k],
                    attendu,
                    "trame ALSA {trame}, canal {canal}, octet {k} : DSD_U32_LE mal empaqueté"
                );
            }
        }
    }
}

/// Le mot vu comme un `u32` : le premier échantillon DSD est le bit 31, dans
/// les deux ordres (c'est la définition ALSA du format).
#[test]
fn le_premier_echantillon_est_le_bit_de_poids_fort_du_mot() {
    // Trames L/R : L = 0x80,0,0,0 (seul le tout premier bit à 1) ; R = 0,0,0,0x01.
    let entree = [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
    for ordre in [OrdreDsdU32::GrosBoutiste, OrdreDsdU32::PetitBoutiste] {
        let mut s = Vec::new();
        assert_eq!(empaqueter(&entree, 2, ordre, &mut s), 8);
        let lire = |o: &[u8]| -> u32 {
            let b = [o[0], o[1], o[2], o[3]];
            match ordre {
                OrdreDsdU32::GrosBoutiste => u32::from_be_bytes(b),
                OrdreDsdU32::PetitBoutiste => u32::from_le_bytes(b),
            }
        };
        assert_eq!(lire(&s[0..4]), 0x8000_0000, "{ordre:?} : canal L");
        assert_eq!(lire(&s[4..8]), 0x0000_0001, "{ordre:?} : canal R");
    }
}

/// Une queue de moins de quatre trames n'est pas empaquetée : elle attend.
#[test]
fn une_queue_incomplete_attend_la_lecture_suivante() {
    let mut s = Vec::new();
    assert_eq!(
        empaqueter(&[1, 2, 3, 4, 5, 6, 7], 2, OrdreDsdU32::GrosBoutiste, &mut s),
        0
    );
    assert!(s.is_empty());
    let mut e = Empaqueteur::new(2, OrdreDsdU32::GrosBoutiste);
    e.pousser(&[1, 2, 3, 4, 5, 6, 7], &mut s);
    assert_eq!(e.en_attente(), 7);
    e.pousser(&[8, 9], &mut s);
    assert_eq!(s, vec![1, 3, 5, 7, 2, 4, 6, 8]);
    assert_eq!(e.en_attente(), 1);
    assert_eq!(e.octets_par_trame(), 8);
}

/// Le motif de repos est le même dans les deux ordres : un mot de repos
/// empaqueté reste 0x69696969.
#[test]
fn le_motif_de_repos_vaut_pour_les_deux_ordres() {
    let repos = [SILENCE_DSD_MSB_FIRST; 8];
    for ordre in [OrdreDsdU32::GrosBoutiste, OrdreDsdU32::PetitBoutiste] {
        let mut s = Vec::new();
        empaqueter(&repos, 2, ordre, &mut s);
        assert_eq!(s, vec![0x69; 8], "{ordre:?}");
    }
}

/// Le choix de l'ordre suit celui de cpal : natif d'abord, l'autre sinon.
#[test]
fn l_ordre_retenu_est_celui_que_cpal_ouvrira() {
    assert_eq!(ordre_retenu(false, false), None);
    assert_eq!(
        ordre_retenu(false, true),
        Some(OrdreDsdU32::GrosBoutiste),
        "Sabaj D5"
    );
    assert_eq!(ordre_retenu(true, false), Some(OrdreDsdU32::PetitBoutiste));
    let natif = if cfg!(target_endian = "little") {
        OrdreDsdU32::PetitBoutiste
    } else {
        OrdreDsdU32::GrosBoutiste
    };
    assert_eq!(ordre_retenu(true, true), Some(natif));
    assert_eq!(OrdreDsdU32::GrosBoutiste.nom_alsa(), "DSD_U32_BE");
}

/// Les cadences : celles du Sabaj D5 relevées par Erik (DSD_U32_BE à 88,2 /
/// 176,4 / 352,8 / 768 kHz… jusqu'à 768 kHz) donnent DSD64 → DSD512, mais
/// pas DSD1024 (1 411 200 Hz refusé, `snd_pcm_hw_params_set_rate: Invalid
/// argument`).
#[test]
fn les_cadences_offertes_se_lisent_sur_la_cadence_de_trame() {
    assert_eq!(cadence_de_trame(2_822_400), 88_200);
    assert_eq!(cadence_de_trame(24_576_000), 768_000);
    assert_eq!(cadence_de_trame(45_158_400), 1_411_200);
    let sabaj = [
        88_200u32, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
    ];
    let c = cadences_offertes(|r| sabaj.contains(&r));
    assert_eq!(
        c,
        vec![
            2_822_400, 3_072_000, 5_644_800, 6_144_000, 11_289_600, 12_288_000, 22_579_200,
            24_576_000
        ]
    );
    assert!(
        !c.contains(&45_158_400),
        "DSD1024 non offert : jamais ouvert en natif"
    );
    assert!(cadences_offertes(|_| false).is_empty());
}
