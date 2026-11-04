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
        // Coupé au milieu d'une page : tronqué.
        let coupe = debuts[n] + 40;
        let c = constat(&octets[..coupe as usize]);
        assert_eq!(
            c.fautes, 0,
            "{nom} coupé à {coupe} : tronqué pris pour corrompu : {c:?}"
        );
        let lieu = c
            .tronque
            .unwrap_or_else(|| panic!("{nom} coupé à {coupe} non signalé"));
        assert_eq!(lieu.numero, n as u64, "{nom} coupé à {coupe}");
        // Coupé pile entre deux pages : seule manque la page de fin, signalée
        // à part (voir le témoin suivant).
        let c = constat(&octets[..debuts[n] as usize]);
        assert_eq!(c.fautes, 0, "{nom} coupé entre deux pages : {c:?}");
        assert!(c.tronque.is_none(), "{nom} : {c:?}");
        let lieu = c
            .fin_absente
            .unwrap_or_else(|| panic!("{nom} coupé à {} non signalé", debuts[n]));
        assert_eq!(
            (lieu.numero, lieu.octet),
            (n as u64, debuts[n]),
            "{nom} : fin de flux absente mal située"
        );
    }
}

#[test]
fn ogg_sans_drapeau_de_fin_n_est_ni_tronque_ni_fautif() {
    for nom in OGG {
        let debuts = sain(nom).debuts;
        let mut octets = fixture(nom);
        let derniere = *debuts.last().unwrap() as usize;
        assert_ne!(octets[derniere + 5] & 0x04, 0, "{nom} : fixture sans EOS");
        // Drapeau EOS retiré et CRC-32 de la page recalculé : la page reste
        // juste, seul manque le drapeau.
        octets[derniere + 5] &= !0x04;
        octets[derniere + 22..derniere + 26].fill(0);
        let crc = octets[derniere..].iter().fold(0, |c, &o| crc32(c, o));
        octets[derniere + 22..derniere + 26].copy_from_slice(&crc.to_le_bytes());
        let c = constat(&octets);
        assert_eq!(c.fautes, 0, "{nom} : page juste signalée fautive : {c:?}");
        assert!(
            c.tronque.is_none(),
            "{nom} : un Ogg aux pages entières sans drapeau de fin déclaré tronqué : {c:?}"
        );
        let lieu = c.fin_absente.expect("drapeau de fin absent non signalé");
        assert_eq!(lieu.octet, octets.len() as u64, "{nom}");
        // Dans le rapport : ni sain ni corrompu, sous un code à part.
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("sans_fin.ogg");
        std::fs::write(&chemin, &octets).unwrap();
        let r = super::super::verifier_rapide(&chemin, || true).unwrap();
        assert_eq!(
            (&r.verdict, r.code.as_str()),
            (&super::super::Verdict::NonVerifiable, "fin_de_flux_absente"),
            "{nom} : {r:?}"
        );
    }
}

/// Bit modifié dans la fréquence, les canaux et la profondeur de STREAMINFO
/// (octets 18 à 21 depuis le début du fichier, après « fLaC » et l'en-tête de
/// bloc), que ne protège aucun CRC.
#[test]
fn flac_streaminfo_abime_n_est_pas_un_fichier_tronque() {
    for nom in FLAC {
        for (octet, masque) in [(19, 0x10), (20, 0x02), (20, 0x01), (21, 0x10)] {
            let mut octets = fixture(nom);
            assert_eq!(&octets[..4], b"fLaC", "{nom}");
            octets[octet] ^= masque;
            let c = constat(&octets);
            assert!(
                c.tronque.is_none(),
                "{nom} entier, octet {octet} de STREAMINFO modifié : signalé tronqué : {c:?}"
            );
            let faute = c.premiere_faute.as_ref().unwrap_or_else(|| {
                panic!("{nom} : octet {octet} de STREAMINFO modifié non signalé : {c:?}")
            });
            assert_eq!(faute.code, "entete_illisible", "{nom} : {c:?}");
        }
    }
}

/// Une zone illisible de plusieurs Mio (STREAMINFO abîmé, reste du fichier
/// sans trame reconnue, ou Ogg dont la suite n'a plus de page) : la pause est
/// entendue en cours de recherche et le tampon reste borné.
#[test]
fn une_zone_illisible_ne_tient_ni_la_pause_ni_la_memoire() {
    const TAILLE: usize = 8 << 20;
    let mut f = fixture("test.flac");
    f[19] ^= 0x10;
    let mut o = fixture("test.ogg");
    let fin_premiere = 27 + usize::from(o[26]);
    let corps: usize = o[27..fin_premiere].iter().map(|&n| usize::from(n)).sum();
    o.truncate(fin_premiere + corps);
    for (nom, mut octets) in [("flac", f), ("ogg", o)] {
        octets.resize(octets.len() + TAILLE, 0x55);
        // Pause demandée après quelques appels : rendue avant la fin.
        let mut appels = 0;
        let analyse = analyser(&octets[..], || {
            appels += 1;
            appels < 8
        })
        .unwrap();
        assert!(
            matches!(analyse, Analyse::Interrompu),
            "{nom} : pause ignorée pendant la recherche : {analyse:?}"
        );
        // Sans pause : la recherche va au bout avec un tampon borné.
        let mut lecteur = Lecteur::new(&octets[..]);
        let analyse = match &octets[..4] {
            b"fLaC" => flac(&mut lecteur, 4, &mut || true),
            _ => ogg(&mut lecteur, &mut || true),
        }
        .unwrap();
        assert!(matches!(analyse, Analyse::Constat(_)), "{nom}");
        assert!(
            lecteur.tampon.len() <= 3 * BLOC,
            "{nom} : {} octets gardés en mémoire pour {} lus",
            lecteur.tampon.len(),
            octets.len()
        );
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
