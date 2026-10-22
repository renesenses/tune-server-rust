//! #6221 — le DSF fabriqué depuis un DSDIFF DST : en-tête exact, données
//! identiques au DSD que décode la sortie USB/ASIO, tranches `Range`.
//!
//! La fixture est l'échantillon FATE `dst-64fs44-2ch.dff` (DSD64 stéréo),
//! celui sur lequel le décodeur a été vérifié bit à bit contre libdstdec.
use super::dsf_depuis_dst::{entete_dsf, type_de_canaux_dsf};

fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// L'en-tête d'un DSF stéréo DSD64 de 3 blocs pleins et un bloc partiel,
/// champ par champ, contre la spécification (valeurs écrites à la main).
#[test]
fn l_entete_dsf_suit_la_specification_champ_par_champ() {
    let octets_par_canal = 3 * 4096 + 100;
    let h = entete_dsf(2, 2_822_400, octets_par_canal);
    let donnees = 4 * 4096 * 2;
    assert_eq!(&h[0..4], b"DSD ");
    assert_eq!(u64_le(&h, 4), 28);
    assert_eq!(u64_le(&h, 12), 92 + donnees, "taille totale du fichier");
    assert_eq!(u64_le(&h, 20), 0, "aucun bloc de métadonnées");
    assert_eq!(&h[28..32], b"fmt ");
    assert_eq!(u64_le(&h, 32), 52);
    assert_eq!(u32_le(&h, 40), 1, "version de format");
    assert_eq!(u32_le(&h, 44), 0, "format id : DSD raw");
    assert_eq!(u32_le(&h, 48), 2, "channel type : stéréo");
    assert_eq!(u32_le(&h, 52), 2, "nombre de canaux");
    assert_eq!(u32_le(&h, 56), 2_822_400);
    assert_eq!(u32_le(&h, 60), 1, "bits per sample = 1 : LSB-first");
    assert_eq!(
        u64_le(&h, 64),
        octets_par_canal * 8,
        "sample count par canal"
    );
    assert_eq!(u32_le(&h, 72), 4096, "block size per channel");
    assert_eq!(u32_le(&h, 76), 0);
    assert_eq!(&h[80..84], b"data");
    assert_eq!(u64_le(&h, 84), 12 + donnees);
    assert_eq!(type_de_canaux_dsf(6), Some(7), "5.1");
    assert_eq!(type_de_canaux_dsf(7), None);
}

#[cfg(feature = "dst")]
mod avec_le_decodeur {
    use super::*;
    use crate::audio::dsf_depuis_dst::{BLOC_DSF, DsfDepuisDst, ENTETE_DSF_LEN};
    use crate::audio::{dff, dsd_brut, dsf};

    fn fixture(nom: &str) -> String {
        format!("{}/tests/fixtures/dsd/{nom}", env!("CARGO_MANIFEST_DIR"))
    }

    fn tout_lire(d: &DsfDepuisDst, debut: u64, longueur: u64) -> Vec<u8> {
        let mut v = Vec::new();
        d.lire(debut, longueur, &mut |m| {
            v.extend_from_slice(&m);
            Ok(())
        })
        .expect("lecture du DSF virtuel");
        v
    }

    /// 🔴 En-tête exact du DSF fabriqué depuis le DST stéréo DSD64 de la
    /// fixture : durée tirée de FRTE (trames × 1/75 s), 4 704 octets DSD par
    /// canal et par trame (2 822 400 / 8 / 75), taille connue d'avance.
    #[test]
    fn entete_exact_du_dsf_fabrique_depuis_le_dst_de_fixture() {
        let chemin = fixture("dst_fate_dsd64_stereo.dff");
        let info = dff::parse_dff(&chemin).unwrap();
        assert!(info.is_dst());
        assert_eq!((info.channels, info.sample_rate), (2, 2_822_400));
        let trames = u64::from(info.dst_frames.unwrap());
        let octets_par_canal = trames * 4704;
        let d = DsfDepuisDst::preparer(&chemin).expect("le DST DSD64 stéréo se prépare");
        assert_eq!(d.octets_par_canal(), octets_par_canal);
        assert_eq!(d.entete(), &entete_dsf(2, 2_822_400, octets_par_canal));
        let blocs = octets_par_canal.div_ceil(4096);
        assert_eq!(d.total(), 92 + blocs * 4096 * 2);
        // Et un lecteur DSF le relit tel qu'annoncé.
        let fichier = tout_lire(&d, 0, d.total());
        assert_eq!(fichier.len() as u64, d.total(), "Content-Length tenu");
        let relu = dsf::parse_dsf_from_bytes(&fichier).expect("DSF lisible");
        assert_eq!(relu.sample_rate, 2_822_400);
        assert_eq!(relu.channels, 2);
        assert_eq!(relu.bits_per_sample, 1);
        assert_eq!(relu.block_size, 4096);
        assert_eq!(relu.total_samples, octets_par_canal * 8);
        assert_eq!(relu.data_offset, ENTETE_DSF_LEN);
        assert_eq!(relu.data_size, blocs * 4096 * 2);
        assert_eq!(d.duree_ms(), trames * 1000 / 75);
    }

    /// 🔴 Les données du DSF sont, bit pour bit, le DSD que la sortie USB/ASIO
    /// reçoit du même fichier (`dsd_brut::lire_dsd_brut`, #5643) — remis en
    /// blocs de 4 096 octets par canal, octets inversés (LSB-first), dernier
    /// bloc complété de zéros.
    #[test]
    fn les_donnees_sont_le_dsd_du_decodage_usb() {
        let chemin = fixture("dst_fate_dsd64_stereo.dff");
        let mut usb = Vec::new();
        let mut premier = true;
        dsd_brut::lire_dsd_brut(&chemin, "dff", dsd_brut::Fenetre::default(), |b| {
            if premier {
                premier = false; // l'en-tête TUNEDSD1
            } else {
                usb.extend_from_slice(&b);
            }
            Ok(())
        })
        .expect("décodage USB");
        let d = DsfDepuisDst::preparer(&chemin).unwrap();
        let c = 2usize;
        assert_eq!(usb.len() as u64, d.octets_par_canal() * c as u64);
        let fichier = tout_lire(&d, 0, d.total());
        let donnees = &fichier[ENTETE_DSF_LEN as usize..];
        let par_canal = d.octets_par_canal() as usize;
        let mut differences = 0usize;
        for i in 0..par_canal {
            let (bloc, dans) = (i / BLOC_DSF, i % BLOC_DSF);
            for canal in 0..c {
                let dsf_octet = donnees[bloc * BLOC_DSF * c + canal * BLOC_DSF + dans];
                if dsf_octet.reverse_bits() != usb[i * c + canal] {
                    differences += 1;
                }
            }
        }
        assert_eq!(differences, 0, "octets DSD différents du décodage USB");
        // Bourrage du dernier bloc : des zéros, comme le veut la spécification.
        let blocs = par_canal.div_ceil(BLOC_DSF);
        for canal in 0..c {
            let debut = (blocs - 1) * BLOC_DSF * c + canal * BLOC_DSF;
            let utiles = par_canal - (blocs - 1) * BLOC_DSF;
            assert!(
                donnees[debut + utiles..debut + BLOC_DSF]
                    .iter()
                    .all(|o| *o == 0),
                "bourrage du canal {canal} non nul"
            );
        }
    }

    /// 🔴 Une requête `Range` au milieu du fichier rend exactement les octets
    /// de ces positions dans le fichier complet — y compris à cheval sur
    /// l'en-tête, au milieu d'un bloc, et sur le dernier octet.
    #[test]
    fn une_tranche_au_milieu_rend_les_octets_du_fichier_complet() {
        let chemin = fixture("dst_fate_dsd64_stereo.dff");
        let d = DsfDepuisDst::preparer(&chemin).unwrap();
        let complet = tout_lire(&d, 0, d.total());
        let t = d.total();
        for (debut, longueur) in [
            (t / 2, 10_000),
            (t / 2 + 4097, 3),
            (50, 200),
            (92 + 8192 * 3 + 4095, 8194),
            (t - 1, 1),
            (t - 5000, 99_999), // borné à la fin
        ] {
            let fin = (debut + longueur).min(t);
            assert_eq!(
                tout_lire(&d, debut, longueur),
                complet[debut as usize..fin as usize],
                "tranche {debut}+{longueur}"
            );
        }
    }

    /// Un DSDIFF NON compressé ne passe pas par ici : il part brut, comme
    /// avant.
    #[test]
    fn un_dff_non_compresse_ne_se_prepare_pas() {
        assert!(DsfDepuisDst::preparer(&fixture("ref_dsd64_stereo.dff")).is_err());
    }
}
