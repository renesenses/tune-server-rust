//! #7847 — témoins du mode rapide sur les fixtures du dépôt : FLAC 16 et
//! 24 bits, Ogg FLAC, Vorbis et Opus.
use super::*;

const FLAC: [&str; 4] = [
    "test.flac",
    "flac/ref_16_44100_mono.flac",
    "flac/ref_16_44100_stereo.flac",
    "flac/ref_24_96000_stereo.flac",
];
const OGG: [&str; 4] = [
    "test.ogg",
    "test_vorbis.ogg",
    "test.opus",
    "test_opus_in.ogg",
];

fn fixture(nom: &str) -> Vec<u8> {
    let chemin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom);
    std::fs::read(&chemin).unwrap_or_else(|e| panic!("{}: {e}", chemin.display()))
}

fn constat(octets: &[u8]) -> Constat {
    match analyser(octets, || true).unwrap() {
        Analyse::Constat(c) => c,
        autre => panic!("constat attendu : {autre:?}"),
    }
}

/// Le constat d'un fichier sain, avec au moins trois trames ou pages pour
/// en abîmer une du milieu.
fn sain(nom: &str) -> Constat {
    let c = constat(&fixture(nom));
    assert_eq!(c.fautes, 0, "{nom} sain signalé fautif : {c:?}");
    assert!(c.tronque.is_none(), "{nom} sain signalé tronqué : {c:?}");
    assert!(c.debuts.len() >= 3, "{nom} : trop peu d'unités : {c:?}");
    assert_eq!(c.unites, c.debuts.len() as u64);
    c
}

#[test]
fn flac_et_ogg_sains_passent() {
    for nom in FLAC {
        assert_eq!(sain(nom).conteneur, Conteneur::Flac);
    }
    for nom in OGG {
        assert_eq!(sain(nom).conteneur, Conteneur::Ogg);
    }
}

#[test]
fn les_fausses_synchros_des_donnees_ne_coupent_pas_les_trames() {
    // Les fixtures portent des motifs 0xFFF8/0xFFF9 hors des débuts de
    // trames : le parcours les a traversés sans faute (témoin ci-dessus).
    let mut fausses = 0;
    for nom in FLAC {
        let octets = fixture(nom);
        let debuts = sain(nom).debuts;
        fausses += octets
            .windows(2)
            .enumerate()
            .filter(|(i, o)| o[0] == 0xFF && o[1] & 0xFE == 0xF8 && !debuts.contains(&(*i as u64)))
            .count();
    }
    assert!(fausses > 0, "aucune fausse synchro dans les fixtures");
}

#[test]
fn flac_octet_modifie_est_localise_a_sa_trame() {
    for nom in FLAC {
        let debuts = sain(nom).debuts;
        let n = debuts.len() / 2;
        let mut octets = fixture(nom);
        let milieu = (debuts[n] + debuts[n + 1]) / 2;
        octets[milieu as usize] ^= 0x10;
        let c = constat(&octets);
        let faute = c
            .premiere_faute
            .as_ref()
            .unwrap_or_else(|| panic!("{nom} : octet {milieu} modifié non détecté : {c:?}"));
        assert_eq!(
            (faute.code, faute.lieu.numero, faute.lieu.octet),
            ("crc_trame", n as u64, debuts[n]),
            "{nom} : faute mal localisée : {c:?}"
        );
        assert!(faute.lieu.echantillon.is_some());
        assert_eq!(c.fautes, 1, "{nom} : une seule trame abîmée : {c:?}");
        assert!(c.tronque.is_none(), "{nom} : abîmé n'est pas tronqué");
    }
}

#[test]
fn flac_entete_de_trame_abime_est_localise_a_sa_trame() {
    for nom in FLAC {
        let debuts = sain(nom).debuts;
        let n = debuts.len() / 2;
        let mut octets = fixture(nom);
        // L'octet de la taille de bloc et de la fréquence : CRC-8 faux.
        octets[debuts[n] as usize + 2] ^= 0x01;
        let c = constat(&octets);
        let faute = c
            .premiere_faute
            .as_ref()
            .expect("en-tête abîmé non détecté");
        assert_eq!(
            (faute.lieu.numero, faute.lieu.octet),
            (n as u64, debuts[n]),
            "{nom} : en-tête abîmé mal localisé : {c:?}"
        );
        assert_eq!(c.fautes, 1, "{nom} : {c:?}");
    }
}

#[test]
fn ogg_octet_modifie_est_localise_a_sa_page() {
    for nom in OGG {
        let debuts = sain(nom).debuts;
        let n = debuts.len() / 2;
        let mut octets = fixture(nom);
        let fin = debuts.get(n + 1).copied().unwrap_or(octets.len() as u64);
        // Dans le corps de la page, après son en-tête et sa table.
        let milieu = (fin - 8) as usize;
        octets[milieu] ^= 0x10;
        let c = constat(&octets);
        let faute = c
            .premiere_faute
            .as_ref()
            .unwrap_or_else(|| panic!("{nom} : octet {milieu} modifié non détecté : {c:?}"));
        assert_eq!(
            (faute.code, faute.lieu.numero, faute.lieu.octet),
            ("crc_page", n as u64, debuts[n]),
            "{nom} : faute mal localisée : {c:?}"
        );
        assert_eq!(c.fautes, 1, "{nom} : une seule page abîmée : {c:?}");
        assert!(c.tronque.is_none(), "{nom} : abîmé n'est pas tronqué");
    }
}

#[test]
fn flac_tronque_est_signale_tronque_pas_corrompu() {
    for nom in FLAC {
        let debuts = sain(nom).debuts;
        let n = debuts.len() / 2;
        let octets = fixture(nom);
        // Coupé au milieu d'une trame, puis pile entre deux trames.
        for coupe in [(debuts[n] + debuts[n + 1]) / 2, debuts[n]] {
            let c = constat(&octets[..coupe as usize]);
            assert_eq!(
                c.fautes, 0,
                "{nom} coupé à {coupe} : tronqué pris pour corrompu : {c:?}"
            );
            let lieu = c
                .tronque
                .unwrap_or_else(|| panic!("{nom} coupé à {coupe} non signalé"));
            assert_eq!(lieu.numero, n as u64, "{nom} coupé à {coupe}");
        }
    }
}

#[test]
fn ogg_tronque_est_signale_tronque_pas_corrompu() {
    for nom in OGG {
        let debuts = sain(nom).debuts;
        let n = debuts.len() / 2;
        let octets = fixture(nom);
        for coupe in [debuts[n] + 40, debuts[n]] {
            let c = constat(&octets[..coupe as usize]);
            assert_eq!(
                c.fautes, 0,
                "{nom} coupé à {coupe} : tronqué pris pour corrompu : {c:?}"
            );
            let lieu = c
                .tronque
                .unwrap_or_else(|| panic!("{nom} coupé à {coupe} non signalé"));
            assert_eq!(lieu.numero, n as u64, "{nom} coupé à {coupe}");
        }
    }
}

#[test]
fn ni_flac_ni_ogg_n_est_pas_applicable() {
    let wav = b"RIFF\x24\0\0\0WAVEfmt ".to_vec();
    assert!(matches!(
        analyser(&wav[..], || true).unwrap(),
        Analyse::NonApplicable
    ));
    assert!(matches!(
        analyser(&b""[..], || true).unwrap(),
        Analyse::NonApplicable
    ));
}

#[test]
fn une_pause_ne_rend_aucun_verdict() {
    let octets = fixture("test.flac");
    assert!(matches!(
        analyser(&octets[..], || false).unwrap(),
        Analyse::Interrompu
    ));
}

#[test]
fn les_sommes_suivent_les_specifications() {
    // Valeurs de contrôle « 123456789 » : CRC-8/SMBUS 0xF4, CRC-16/UMTS
    // 0xFEE8, CRC-32/POSIX sans son xor final 0x89A1897F.
    let message = b"123456789";
    assert_eq!(crc8(message), 0xF4);
    assert_eq!(message.iter().fold(0, |c, &o| crc16(c, o)), 0xFEE8);
    assert_eq!(message.iter().fold(0, |c, &o| crc32(c, o)), 0x89A1_897F);
}
