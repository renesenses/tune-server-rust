//! #9184 — le DSF fabriqué depuis une tranche d'ISO SACD : en-tête exact,
//! données identiques au DSD de la tranche (bits inversés, blocs de 4 096
//! octets par canal), tranches `Range` cohérentes.
//!
//! L'image est fabriquée (`sacd::fabrique`) : ses trames portent un DSD
//! CONNU (`octets_attendus`), si bien que l'épreuve compare des octets.
use super::dsf;
use super::dsf_depuis_dst::{BLOC_DSF, ENTETE_DSF_LEN, entete_dsf};
use super::dsf_depuis_iso::DsfDepuisIso;
use super::sacd::fabrique::{ImageFabriquee, ecrire, octets_attendus};
use super::sacd::{OCTETS_PAR_TRAME_ET_CANAL, trames_en_ms};

fn tout_lire(d: &DsfDepuisIso, debut: u64, longueur: u64) -> Vec<u8> {
    let mut v = Vec::new();
    d.lire(debut, longueur, &mut |m| {
        v.extend_from_slice(&m);
        Ok(())
    })
    .expect("lecture du DSF virtuel");
    v
}

/// Le DSD MSB-first entrelacé (forme DSDIFF) que portent les données d'un
/// DSF stéréo, `par_canal` octets par voie.
fn dsd_du_dsf(donnees: &[u8], par_canal: usize) -> Vec<u8> {
    let c = 2;
    let mut v = Vec::with_capacity(par_canal * c);
    for i in 0..par_canal {
        let (bloc, dans) = (i / BLOC_DSF, i % BLOC_DSF);
        for canal in 0..c {
            v.push(donnees[bloc * BLOC_DSF * c + canal * BLOC_DSF + dans].reverse_bits());
        }
    }
    v
}

struct Image {
    _dir: tempfile::TempDir,
    chemin: std::path::PathBuf,
    bornes: Vec<(u32, u32)>,
}

fn image(premiere_trame: u32) -> Image {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("Kind of Blue.iso");
    let mut f = ImageFabriquee::deux_pistes();
    f.premiere_trame = premiere_trame;
    let bornes = ecrire(&chemin, &f);
    Image {
        _dir: dir,
        chemin,
        bornes,
    }
}

fn preparer(i: &Image, piste: usize) -> DsfDepuisIso {
    let (debut, fin) = i.bornes[piste];
    DsfDepuisIso::preparer(&i.chemin, trames_en_ms(debut), Some(trames_en_ms(fin)))
        .expect("la tranche stéréo DSD se prépare")
}

/// 🔴 La piste 2 (7 trames) : un DSF valide — en-tête de la spécification,
/// fréquence, nombre d'échantillons — et des données qui sont, octet pour
/// octet, le DSD de la TRANCHE (ni la piste 1, ni rien au-delà).
#[test]
fn la_tranche_d_iso_devient_un_dsf_exact() {
    for premiere in [0, 150] {
        let i = image(premiere);
        let (debut, fin) = i.bornes[1];
        let d = preparer(&i, 1);
        let par_canal = u64::from(fin - debut) * OCTETS_PAR_TRAME_ET_CANAL as u64;
        assert_eq!(d.octets_par_canal(), par_canal);
        assert_eq!((d.canaux(), d.cadence()), (2, 2_822_400));
        assert_eq!(d.entete(), &entete_dsf(2, 2_822_400, par_canal));
        let blocs = par_canal.div_ceil(BLOC_DSF as u64);
        assert_eq!(d.total(), ENTETE_DSF_LEN + blocs * BLOC_DSF as u64 * 2);

        let fichier = tout_lire(&d, 0, d.total());
        assert_eq!(fichier.len() as u64, d.total(), "Content-Length tenu");
        let relu = dsf::parse_dsf_from_bytes(&fichier).expect("DSF lisible");
        assert_eq!(relu.sample_rate, 2_822_400);
        assert_eq!(relu.channels, 2);
        assert_eq!(relu.bits_per_sample, 1, "LSB-first");
        assert_eq!(relu.block_size, 4096);
        assert_eq!(relu.total_samples, par_canal * 8, "échantillons par canal");
        assert_eq!(relu.data_offset, ENTETE_DSF_LEN);

        let donnees = &fichier[ENTETE_DSF_LEN as usize..];
        let dsd = dsd_du_dsf(donnees, par_canal as usize);
        assert!(
            dsd == octets_attendus(debut, fin),
            "données du DSF ≠ DSD de la tranche (première trame {premiere})"
        );
        // Bourrage du dernier bloc : des zéros.
        let utiles = par_canal as usize - (blocs as usize - 1) * BLOC_DSF;
        for canal in 0..2 {
            let a = (blocs as usize - 1) * BLOC_DSF * 2 + canal * BLOC_DSF;
            assert!(donnees[a + utiles..a + BLOC_DSF].iter().all(|&o| o == 0));
        }
    }
}

/// 🔴 Range : n'importe quelle fenêtre `[a, b)` rend exactement les octets
/// correspondants du fichier entier — en-tête, milieu de bloc, au-delà
/// d'une frontière de trame (4 704) et de bloc (4 096), fin du fichier.
#[test]
fn les_fenetres_range_sont_coherentes_avec_le_fichier_entier() {
    let i = image(0);
    let d = preparer(&i, 1);
    let entier = tout_lire(&d, 0, d.total());
    let t = d.total();
    let s = BLOC_DSF as u64 * 2;
    for (a, b) in [
        (0, 50),
        (40, 200),
        (ENTETE_DSF_LEN, ENTETE_DSF_LEN + 1),
        (ENTETE_DSF_LEN + s, ENTETE_DSF_LEN + 2 * s),
        (ENTETE_DSF_LEN + 3 * s + 17, ENTETE_DSF_LEN + 5 * s + 3),
        (ENTETE_DSF_LEN + 4704 + 5, t - 7),
        (t - 1000, t),
        (t - 1, t + 500),
    ] {
        let part = tout_lire(&d, a, b - a);
        assert!(
            part[..] == entier[a as usize..(b.min(t)) as usize],
            "fenêtre {a}..{b} incohérente"
        );
    }
    assert!(tout_lire(&d, t, 10).is_empty());
}

/// Une tranche sans fin, vide, ou une image DST : refus — le chemin PCM
/// reste celui d'avant.
#[test]
fn ce_qui_ne_se_prepare_pas_est_refuse() {
    let i = image(0);
    assert!(DsfDepuisIso::preparer(&i.chemin, 0, None).is_err());
    assert!(DsfDepuisIso::preparer(&i.chemin, 40, Some(40)).is_err());
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("dst.iso");
    let mut f = ImageFabriquee::deux_pistes();
    f.stereo_dst = true;
    ecrire(&dst, &f);
    assert!(DsfDepuisIso::preparer(&dst, 0, Some(trames_en_ms(3))).is_err());
}
