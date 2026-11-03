//! #6353 — témoins de la reconnaissance des flux Dolby, sur des en-têtes
//! FABRIQUÉS bit à bit d'après les normes (ATSC A/52 et son annexe E pour
//! l'AC-3 et l'E-AC-3, la synchronisation majeure MLP/TrueHD), puis dans un
//! vrai conteneur Matroska fabriqué par `muxer_de_test`. ffmpeg est interdit,
//! et aucun fichier Dolby n'est versionné : les trames se construisent ici.

use super::*;

/// Écrit des champs de N bits, poids fort d'abord.
struct Ecrivain {
    octets: Vec<u8>,
    bits: usize,
}

impl Ecrivain {
    fn new() -> Self {
        Self {
            octets: Vec::new(),
            bits: 0,
        }
    }

    fn ecrire(&mut self, n: u32, v: u32) -> &mut Self {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.octets.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            let dernier = self.octets.len() - 1;
            self.octets[dernier] |= bit << (7 - (self.bits % 8));
            self.bits += 1;
        }
        self
    }

    /// Complète par des zéros jusqu'à `taille` octets.
    fn jusqu_a(&mut self, taille: usize) -> Vec<u8> {
        let mut v = self.octets.clone();
        assert!(v.len() <= taille, "en-tête plus long que la trame");
        v.resize(taille, 0);
        v
    }
}

/// Une trame AC-3 : `fscod`, `frmsizecod`, `bsmod`, `acmod`, `lfeon`.
fn trame_ac3(fscod: u32, frmsizecod: u32, bsmod: u32, acmod: u32, lfe: bool) -> Vec<u8> {
    let mut e = Ecrivain::new();
    e.ecrire(16, 0x0B77).ecrire(16, 0); // crc1
    e.ecrire(2, fscod).ecrire(6, frmsizecod);
    e.ecrire(5, 8).ecrire(3, bsmod).ecrire(3, acmod);
    if acmod & 1 == 1 && acmod != 1 {
        e.ecrire(2, 0);
    }
    if acmod & 4 == 4 {
        e.ecrire(2, 0);
    }
    if acmod == 2 {
        e.ecrire(2, 0);
    }
    e.ecrire(1, u32::from(lfe));
    let mots = TAILLES_AC3[(frmsizecod / 2) as usize][fscod as usize] as usize;
    e.jusqu_a(mots * 2)
}

/// Une trame E-AC-3 de `frmsiz` (taille = (frmsiz+1)×2 octets).
fn trame_eac3(strmtyp: u32, frmsiz: u32, acmod: u32, lfe: bool, chanmap: Option<u32>) -> Vec<u8> {
    let mut e = Ecrivain::new();
    e.ecrire(16, 0x0B77)
        .ecrire(2, strmtyp)
        .ecrire(3, 0)
        .ecrire(11, frmsiz);
    e.ecrire(2, 0).ecrire(2, 3); // fscod 48 kHz, numblkscod 6 blocs
    e.ecrire(3, acmod).ecrire(1, u32::from(lfe)).ecrire(5, 16);
    e.ecrire(5, 31).ecrire(1, 0); // dialnorm, compre
    if acmod == 0 {
        e.ecrire(5, 31).ecrire(1, 0);
    }
    if strmtyp == 1 {
        match chanmap {
            Some(m) => e.ecrire(1, 1).ecrire(16, m),
            None => e.ecrire(1, 0),
        };
    }
    e.jusqu_a((frmsiz as usize + 1) * 2)
}

/// Une unité d'accès TrueHD avec synchronisation majeure.
fn unite_truehd(debit_code: u32, aff6: u32, aff8: u32, sous_flux: u32) -> Vec<u8> {
    let mots = 40u32;
    let mut e = Ecrivain::new();
    e.ecrire(4, 0xF).ecrire(12, mots).ecrire(16, 0); // nibble, longueur, timing
    e.ecrire(32, 0xF872_6FBA);
    e.ecrire(4, debit_code)
        .ecrire(4, 0)
        .ecrire(2, 0)
        .ecrire(2, 0);
    e.ecrire(5, aff6).ecrire(2, 0).ecrire(13, aff8);
    e.ecrire(16, 0xB752).ecrire(16, 0).ecrire(16, 0);
    e.ecrire(1, 1).ecrire(15, 0x1234).ecrire(4, sous_flux);
    e.jusqu_a(mots as usize * 2)
}

/// L/R, C, LFE, Ls/Rs : 5.1. Plus Lrs/Rrs (bit 6) : 7.1.
const AFF_51: u32 = 0b0_1111;
const AFF_71: u32 = 0b100_1111;

#[test]
fn un_en_tete_ac3_dit_canaux_frequence_et_taille() {
    let t = trame_ac3(0, 30, 0, 7, true); // 448 kb/s, 48 kHz, 3/2 + LFE
    let e = analyser_ac3(&t).expect("en-tête AC-3");
    assert_eq!(e.codec, CodecDolby::Ac3);
    assert_eq!(e.frequence, 48_000);
    assert_eq!((e.canaux, e.lfe), (5, true));
    assert_eq!(e.taille, Some(1792), "448 kb/s à 48 kHz : 896 mots");
    let t = trame_ac3(1, 9, 0, 2, false); // 128 kb/s, 44,1 kHz, stéréo, impair
    let e = analyser_ac3(&t).unwrap();
    assert_eq!((e.frequence, e.canaux, e.lfe), (44_100, 2, false));
    assert_eq!(
        e.taille,
        Some((139 + 1) * 2),
        "un mot de plus à 44,1 kHz impair"
    );
}

#[test]
fn tout_ce_qui_n_est_pas_une_trame_dolby_est_refuse() {
    assert_eq!(analyser_ac3(&[]), None);
    assert_eq!(analyser_ac3(&[0x0B]), None);
    assert_eq!(analyser_ac3(b"fLaC\0\0\0\0\0\0\0\0"), None);
    let mut t = trame_ac3(0, 28, 0, 7, true);
    t[5] = 20 << 3; // bsid 20 : ni AC-3 ni E-AC-3
    assert_eq!(analyser_ac3(&t), None);
    assert_eq!(analyser_truehd(&[0u8; 32]), None);
    let mut u = unite_truehd(0, AFF_51, AFF_71, 4);
    u[12] ^= 0xFF; // signature 0xB752 abîmée (octets 12-13)
    assert_eq!(analyser_truehd(&u), None);
}

#[test]
fn un_eac3_7_1_se_compte_avec_son_sous_flux_dependant() {
    let mut paquet = trame_eac3(0, 255, 7, true, None); // 5.1 indépendant
    paquet.extend(trame_eac3(1, 127, 2, false, Some(1 << (15 - 6)))); // + Lrs/Rrs
    let f = analyser_les_paquets(CodecDolby::Eac3, [paquet.as_slice()]).expect("flux E-AC-3");
    assert_eq!(f.codec, CodecDolby::Eac3);
    assert_eq!(f.frequence, 48_000);
    assert_eq!(f.disposition(), "7.1");
    assert_eq!(
        f.atmos, None,
        "Atmos d'un E-AC-3 : non déterminé à l'étape 1"
    );
    // Sans sous-flux dépendant : 5.1.
    let seul = trame_eac3(0, 255, 7, true, None);
    let f = analyser_les_paquets(CodecDolby::Eac3, [seul.as_slice()]).unwrap();
    assert_eq!(f.disposition(), "5.1");
}

#[test]
fn un_truehd_dit_sa_presentation_la_plus_large_et_atmos() {
    let u = unite_truehd(0, AFF_51, AFF_71, 4);
    let e = analyser_truehd(&u).expect("synchronisation majeure");
    assert_eq!((e.frequence, e.canaux, e.lfe), (48_000, 7, true));
    assert_eq!(e.taille, Some(80));
    let f = analyser_les_paquets(CodecDolby::TrueHd, [u.as_slice()]).unwrap();
    assert_eq!(f.disposition(), "7.1");
    assert_eq!(f.atmos, Some(true));
    assert_eq!(f.decrire(), "Dolby TrueHD 7.1, 48 kHz, Atmos (4 sous-flux)");
    // 5.1 à 96 kHz, trois sous-flux : pas d'Atmos.
    let u = unite_truehd(1, AFF_51, 0, 3);
    let f = analyser_les_paquets(CodecDolby::TrueHd, [u.as_slice()]).unwrap();
    assert_eq!(
        (f.frequence, f.disposition(), f.atmos),
        (96_000, "5.1".into(), Some(false))
    );
    // 44,1 kHz : le bit 8 du code.
    assert_eq!(
        analyser_truehd(&unite_truehd(8, AFF_51, 0, 2))
            .unwrap()
            .frequence,
        44_100
    );
}

#[test]
fn le_premier_paquet_avec_synchronisation_l_emporte() {
    // Les unités sans synchronisation majeure ne disent rien : on avance.
    let mineure = vec![0xF0u8, 0x28, 0, 0, 1, 2, 3, 4];
    let majeure = unite_truehd(0, AFF_51, AFF_71, 4);
    let f = analyser_les_paquets(
        CodecDolby::TrueHd,
        [mineure.as_slice(), mineure.as_slice(), majeure.as_slice()],
    );
    assert_eq!(f.map(|f| f.canaux), Some(8));
    assert_eq!(
        analyser_les_paquets(CodecDolby::TrueHd, [mineure.as_slice()]),
        None
    );
}

/// Un en-tête qui promet plus d'octets que le paquet n'en porte n'est pas une
/// trame : huit octets qui commencent par `0x0B77` ne font pas un AC-3.
#[test]
fn une_trame_tronquee_n_est_pas_reconnue() {
    let t = trame_ac3(0, 30, 0, 7, true);
    assert!(analyser_les_paquets(CodecDolby::Ac3, [t.as_slice()]).is_some());
    assert_eq!(
        analyser_les_paquets(CodecDolby::Ac3, [&t[..t.len() - 1]]),
        None
    );
    let u = unite_truehd(0, AFF_51, AFF_71, 4);
    assert_eq!(analyser_les_paquets(CodecDolby::TrueHd, [&u[..40]]), None);
    let e = trame_eac3(0, 255, 7, true, None);
    assert_eq!(analyser_les_paquets(CodecDolby::Eac3, [&e[..100]]), None);
}

#[test]
fn la_porteuse_iec61937_suit_le_codec() {
    let f = |codec, frequence, canaux| FluxDolby {
        codec,
        frequence,
        canaux,
        lfe: true,
        atmos: None,
    };
    assert_eq!(
        porteuse_iec61937(&f(CodecDolby::Ac3, 48_000, 6)),
        Porteuse {
            canaux: 2,
            frequence: 48_000
        }
    );
    assert_eq!(
        porteuse_iec61937(&f(CodecDolby::Eac3, 48_000, 8)),
        Porteuse {
            canaux: 2,
            frequence: 192_000
        }
    );
    assert_eq!(
        porteuse_iec61937(&f(CodecDolby::TrueHd, 48_000, 8)),
        Porteuse {
            canaux: 8,
            frequence: 192_000
        }
    );
}

#[test]
fn une_rafale_ac3_porte_ses_preambules_sa_trame_et_du_silence() {
    let t = trame_ac3(0, 28, 3, 7, true);
    let e = analyser_ac3(&t).unwrap();
    let r = rafale_iec61937(&e, &t).expect("rafale AC-3");
    assert_eq!(r.len(), 6144, "1 536 échantillons stéréo 16 bits");
    let mot = |i: usize| u16::from_le_bytes([r[2 * i], r[2 * i + 1]]);
    assert_eq!(mot(0), 0xF872, "Pa");
    assert_eq!(mot(1), 0x4E1F, "Pb");
    assert_eq!(mot(2), 0x0001 | (3 << 8), "Pc : AC-3, bsmod 3");
    assert_eq!(mot(3) as usize, t.len() * 8, "Pd : longueur en bits");
    // La trame en mots : le synchronisme 0x0B77 en tête de charge.
    assert_eq!(mot(4), 0x0B77);
    for i in (4 + t.len() / 2)..(6144 / 2) {
        assert_eq!(mot(i), 0, "silence après la trame (mot {i})");
    }
}

#[test]
fn une_rafale_eac3_compte_en_octets_et_truehd_attend_le_mat() {
    let t = trame_eac3(0, 255, 7, true, None);
    let e = analyser_ac3(&t).unwrap();
    let r = rafale_iec61937(&e, &t).expect("rafale E-AC-3");
    assert_eq!(r.len(), 24_576);
    assert_eq!(u16::from_le_bytes([r[4], r[5]]), 0x0015);
    assert_eq!(u16::from_le_bytes([r[6], r[7]]) as usize, t.len());
    let u = unite_truehd(0, AFF_51, AFF_71, 4);
    assert_eq!(rafale_iec61937(&analyser_truehd(&u).unwrap(), &u), None);
    // Trop grande pour sa rafale : refusée, jamais tronquée.
    assert_eq!(
        rafale_iec61937(&analyser_ac3(&t).unwrap(), &vec![0; 30_000]),
        None
    );
}

#[test]
fn le_chemin_du_signal_dit_que_le_passthrough_manque() {
    let f = analyser_les_paquets(
        CodecDolby::TrueHd,
        [unite_truehd(0, AFF_51, AFF_71, 4).as_slice()],
    )
    .unwrap();
    let c = chemin_du_signal(&f);
    assert_eq!(
        c.iter().map(|e| e.name).collect::<Vec<_>>(),
        ["Source", "Démuxage", "Transport"]
    );
    assert!(
        c[0].description.contains("TrueHD 7.1, 48 kHz, Atmos"),
        "{:?}",
        c[0]
    );
    assert!(c[0].bit_perfect && c[1].bit_perfect);
    let t = &c[2];
    assert!(!t.bit_perfect);
    assert_eq!(t.code, Some(CODE_PASSTHROUGH_NON_PRIS_EN_CHARGE));
    assert!(
        t.description.contains("8 canaux à 192 kHz"),
        "{}",
        t.description
    );
    assert!(t.description.contains("HDMI (HBR)"), "{}", t.description);
    // Sérialisé comme une étape de `signal_path`.
    let v = serde_json::to_value(t).unwrap();
    assert_eq!(v["name"], "Transport");
    assert_eq!(v["bit_perfect"], false);
    assert_eq!(v["code"], CODE_PASSTHROUGH_NON_PRIS_EN_CHARGE);
}

// ── Dans un vrai conteneur ─────────────────────────────────────────────────

fn mkv(codec_id: &str, trames: Vec<Vec<u8>>, sample_rate: u32, channels: u16) -> Vec<u8> {
    use crate::audio::matroska::muxer_de_test::{Piste, Trame, matroska};
    let trames: Vec<Trame> = trames
        .into_iter()
        .enumerate()
        .map(|(i, octets)| Trame {
            ts_ms: i as u64 * 32,
            octets,
        })
        .collect();
    matroska(
        "matroska",
        &Piste {
            codec_id,
            codec_private: None,
            sample_rate,
            channels,
            bit_depth: None,
            duree_trame_ns: Some(32_000_000),
        },
        trames.len() as f64 * 32.0,
        &trames,
        1,
        false,
        &[("TITLE", "Concert témoin")],
        &[],
    )
}

fn ecrire(
    dossier: &crate::test_scratch::ScratchDir,
    nom: &str,
    octets: &[u8],
) -> std::path::PathBuf {
    let chemin = dossier.join(nom);
    std::fs::write(&chemin, octets).unwrap();
    chemin
}

#[test]
fn la_sonde_matroska_lit_le_flux_truehd_sans_le_decoder() {
    let dossier = crate::test_scratch::scratch_dir("tune-6353-truehd");
    let mineure = vec![0xF0u8, 0x28, 0, 0, 1, 2, 3, 4];
    let chemin = ecrire(
        &dossier,
        "concert.mkv",
        &mkv(
            "A_TRUEHD",
            vec![mineure.clone(), unite_truehd(0, AFF_51, AFF_71, 4), mineure],
            48_000,
            8,
        ),
    );
    let sonde = crate::audio::matroska::sonder(&chemin).expect("sonde");
    assert_eq!(
        sonde.piste,
        crate::audio::matroska::PisteAudio::NonDecodable {
            codec: "truehd".into()
        }
    );
    let f = sonde.flux_dolby.expect("flux TrueHD reconnu");
    assert_eq!(f.decrire(), "Dolby TrueHD 7.1, 48 kHz, Atmos (4 sous-flux)");

    // Le rapport de scan le dit, sous la MÊME clé qu'avant.
    let refus = crate::audio::support::decoder_rejection(&chemin).expect("refusé");
    assert_eq!(refus.report_key, "mkv-codec-non-decodable-truehd");
    assert!(
        refus.reason.contains("Dolby TrueHD 7.1, 48 kHz, Atmos"),
        "{}",
        refus.reason
    );
    assert!(refus.reason.contains("passthrough"), "{}", refus.reason);
}

#[test]
fn la_sonde_matroska_lit_le_flux_eac3_7_1() {
    let dossier = crate::test_scratch::scratch_dir("tune-6353-eac3");
    let mut paquet = trame_eac3(0, 255, 7, true, None);
    paquet.extend(trame_eac3(1, 127, 2, false, Some(1 << (15 - 6))));
    let chemin = ecrire(
        &dossier,
        "film.mkv",
        &mkv("A_EAC3", vec![paquet.clone(), paquet], 48_000, 8),
    );
    let sonde = crate::audio::matroska::sonder(&chemin).expect("sonde");
    let f = sonde.flux_dolby.expect("flux E-AC-3 reconnu");
    assert_eq!(f.decrire(), "Dolby Digital Plus (E-AC-3) 7.1, 48 kHz");
    let refus = crate::audio::support::decoder_rejection(&chemin).expect("refusé");
    assert_eq!(refus.report_key, "mkv-codec-non-decodable-eac3");
}

/// Un flux illisible garde le motif d'avant, mot pour mot : rien n'est
/// inventé quand les en-têtes ne parlent pas.
#[test]
fn un_flux_sans_en_tete_lisible_garde_le_motif_d_avant() {
    let dossier = crate::test_scratch::scratch_dir("tune-6353-muet");
    let chemin = ecrire(
        &dossier,
        "muet.mkv",
        &mkv("A_TRUEHD", vec![vec![0u8; 64]; 3], 48_000, 8),
    );
    let sonde = crate::audio::matroska::sonder(&chemin).expect("sonde");
    assert_eq!(sonde.flux_dolby, None);
    let refus = crate::audio::support::decoder_rejection(&chemin).expect("refusé");
    assert_eq!(refus.report_key, "mkv-codec-non-decodable-truehd");
    assert!(
        refus.reason.contains("aucun décodeur livré ne la lit"),
        "{}",
        refus.reason
    );
}
