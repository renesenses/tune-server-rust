//! #8105 — témoins de la fausse fin de fichier : un FLAC intact suivi
//! d'étiquettes finales (ID3v1, APEv2, Lyrics3v2), aux blocs de métadonnées
//! variés, lu d'un coup ou par lectures courtes, est sain ; un FLAC réellement
//! coupé est « tronqué », avec sa position et sa taille réelle.
use super::*;
use md5::{Digest, Md5};
use std::io::Cursor;

/// 12 000 échantillons stéréo 16/44,1 : deux trames de 4 096 et une dernière
/// de 3 808, la trame courte que perdait le démultiplexeur.
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

/// Ajoute après les métadonnées existantes un VORBIS_COMMENT de `commentaire`
/// octets de texte et un PADDING de `bourrage` octets, comme libFLAC 1.2.1
/// (bourrage de 8 192 par défaut).
fn avec_blocs(f: &[u8], commentaire: usize, bourrage: usize) -> Vec<u8> {
    let mut pos = 4;
    let dernier = loop {
        let longueur = u32::from_be_bytes([0, f[pos + 1], f[pos + 2], f[pos + 3]]) as usize;
        if f[pos] & 0x80 != 0 {
            break pos;
        }
        pos += 4 + longueur;
    };
    let fin_meta = dernier
        + 4
        + u32::from_be_bytes([0, f[dernier + 1], f[dernier + 2], f[dernier + 3]]) as usize;
    let mut sortie = f[..fin_meta].to_vec();
    sortie[dernier] &= 0x7f;
    let vendeur = b"reference libFLAC 1.2.1 20070917";
    let texte = format!("COMMENT={}", "x".repeat(commentaire));
    let mut corps = Vec::new();
    corps.extend_from_slice(&(vendeur.len() as u32).to_le_bytes());
    corps.extend_from_slice(vendeur);
    corps.extend_from_slice(&1u32.to_le_bytes());
    corps.extend_from_slice(&(texte.len() as u32).to_le_bytes());
    corps.extend_from_slice(texte.as_bytes());
    sortie.push(4);
    sortie.extend_from_slice(&(corps.len() as u32).to_be_bytes()[1..]);
    sortie.extend_from_slice(&corps);
    sortie.push(0x80 | 1);
    sortie.extend_from_slice(&(bourrage as u32).to_be_bytes()[1..]);
    sortie.resize(sortie.len() + bourrage, 0);
    sortie.extend_from_slice(&f[fin_meta..]);
    sortie
}

fn id3v1() -> Vec<u8> {
    let mut t = b"TAG".to_vec();
    t.resize(128, b' ');
    t
}

fn apev2(avec_entete: bool) -> Vec<u8> {
    let mut element = Vec::new();
    let valeur = b"Beh\xc3\xb6lder";
    element.extend_from_slice(&(valeur.len() as u32).to_le_bytes());
    element.extend_from_slice(&0u32.to_le_bytes());
    element.extend_from_slice(b"Artist\0");
    element.extend_from_slice(valeur);
    let bloc = |drapeaux: u32| {
        let mut b = b"APETAGEX".to_vec();
        b.extend_from_slice(&2000u32.to_le_bytes());
        b.extend_from_slice(&((element.len() + 32) as u32).to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&drapeaux.to_le_bytes());
        b.extend_from_slice(&[0; 8]);
        b
    };
    let mut t = Vec::new();
    if avec_entete {
        t.extend(bloc((1 << 31) | (1 << 29)));
    }
    t.extend_from_slice(&element);
    t.extend(bloc(if avec_entete { 1 << 31 } else { 0 }));
    t
}

fn lyrics3v2() -> Vec<u8> {
    let corps = b"LYRICSBEGININD00002 1";
    let mut t = corps.to_vec();
    t.extend_from_slice(format!("{:06}", corps.len()).as_bytes());
    t.extend_from_slice(b"LYRICS200");
    t
}

/// Les étiquettes finales rencontrées : seules, combinées, dans l'ordre que
/// les tagueurs écrivent (APEv2 ou Lyrics3v2 avant un ID3v1).
fn queues() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("aucune", Vec::new()),
        ("id3v1", id3v1()),
        ("apev2", apev2(true)),
        ("apev2 sans en-tête", apev2(false)),
        ("apev2 + id3v1", [apev2(true), id3v1()].concat()),
        ("lyrics3v2 + id3v1", [lyrics3v2(), id3v1()].concat()),
    ]
}

/// Les variantes de métadonnées : celles de l'encodeur, puis un bourrage de
/// libFLAC 1.2.1 et un VORBIS_COMMENT long.
fn variantes() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("streaminfo seul", flac()),
        ("bourrage 8192", avec_blocs(&flac(), 20, 8192)),
        ("commentaire long", avec_blocs(&flac(), 70_000, 0)),
        (
            "commentaire long + bourrage",
            avec_blocs(&flac(), 300_000, 65_536),
        ),
    ]
}

/// Une source qui rend des lectures courtes (1 à 7 octets, en tournant) sans
/// être en fin de fichier, comme `ReadFile` sous Windows peut le faire.
struct LecturesCourtes {
    octets: Cursor<Vec<u8>>,
    tour: usize,
}

impl LecturesCourtes {
    fn new(octets: &[u8]) -> Self {
        Self {
            octets: Cursor::new(octets.to_vec()),
            tour: 0,
        }
    }
}

impl Read for LecturesCourtes {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.tour += 1;
        let n = buf.len().min(1 + self.tour % 7);
        self.octets.read(&mut buf[..n])
    }
}

impl Seek for LecturesCourtes {
    fn seek(&mut self, vers: SeekFrom) -> io::Result<u64> {
        self.octets.seek(vers)
    }
}

fn auditer_par_lectures_courtes(octets: &[u8]) -> Resultat {
    verifier_source(
        LecturesCourtes::new(octets),
        octets.len() as u64,
        Some("flac"),
        || Ok(LecturesCourtes::new(octets)),
        || true,
    )
    .unwrap()
}

fn auditer(octets: &[u8], mode: Mode) -> Resultat {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("temoin.flac");
    std::fs::write(&chemin, octets).unwrap();
    verifier_selon(mode, &chemin, || true).unwrap()
}

#[test]
fn flac_intact_suivi_d_etiquettes_finales_est_sain() {
    for (meta, f) in variantes() {
        for (queue, etiquettes) in queues() {
            let fichier = [f.clone(), etiquettes].concat();
            let r = auditer(&fichier, Mode::Complet);
            assert_eq!(
                (r.verdict.clone(), r.trames_decodees),
                (Verdict::Sain, 12000),
                "FLAC intact ({meta}, étiquette finale : {queue}) déclaré corrompu : {r:?}"
            );
            let r = auditer(&fichier, Mode::Rapide);
            assert_eq!(
                r.verdict,
                Verdict::Sain,
                "mode rapide ({meta}, étiquette finale : {queue}) : {r:?}"
            );
        }
    }
}

#[test]
fn flac_intact_lu_par_lectures_courtes_est_sain() {
    for (meta, f) in variantes() {
        for (queue, etiquettes) in queues() {
            let fichier = [f.clone(), etiquettes].concat();
            let r = auditer_par_lectures_courtes(&fichier);
            assert_eq!(
                (r.verdict.clone(), r.trames_decodees),
                (Verdict::Sain, 12000),
                "lecture courte prise pour une fin de fichier ({meta}, {queue}) : {r:?}"
            );
        }
    }
}

#[test]
fn flac_reellement_tronque_donne_position_et_taille_reelle() {
    for (meta, f) in variantes() {
        // Coupé au milieu des données audio (dans la deuxième trame), puis la
        // même coupe suivie d'un ID3v1 : une étiquette finale ne masque pas
        // une troncature. Après STREAMINFO, `flac()` n'a que de l'audio.
        let coupe = f.len() - (flac().len() - 42) / 2;
        for (queue, etiquettes) in [("aucune", Vec::new()), ("id3v1", id3v1())] {
            let fichier = [f[..coupe].to_vec(), etiquettes].concat();
            for r in [
                auditer(&fichier, Mode::Complet),
                auditer_par_lectures_courtes(&fichier),
            ] {
                assert_eq!(r.verdict, Verdict::Corrompu, "troncature ignorée : {r:?}");
                assert_eq!(
                    r.code, "tronque",
                    "troncature mal nommée ({meta}, {queue}) : {r:?}"
                );
                let erreur = r.erreur.clone().unwrap();
                assert!(
                    erreur.contains(&format!("taille réelle : {} octets", fichier.len())),
                    "taille réelle absente du message : {erreur}"
                );
                assert!(erreur.contains("octet "), "position absente : {erreur}");
                assert!(r.position_trames.is_some_and(|p| p < 12000), "{r:?}");
            }
            let r = auditer(&fichier, Mode::Rapide);
            assert_eq!(r.code, "tronque", "mode rapide ({meta}, {queue}) : {r:?}");
            assert!(
                r.erreur
                    .unwrap()
                    .contains(&format!("taille réelle : {} octets", fichier.len())),
            );
        }
    }
}

#[test]
fn derniere_trame_abimee_n_est_pas_dite_tronquee() {
    // Un octet de la dernière trame change : le fichier a toute sa longueur,
    // il est corrompu mais pas « tronqué ».
    let mut fichier = flac();
    let n = fichier.len();
    fichier[n - 10] ^= 0x55;
    let r = auditer(&fichier, Mode::Complet);
    assert_eq!(r.verdict, Verdict::Corrompu, "{r:?}");
    assert_ne!(r.code, "tronque", "fichier entier déclaré tronqué : {r:?}");
}

#[test]
fn fin_des_donnees_ne_retire_que_des_etiquettes() {
    let f = flac();
    for (queue, etiquettes) in queues() {
        let fichier = [f.clone(), etiquettes].concat();
        assert_eq!(
            fin_des_donnees(&mut Cursor::new(&fichier), fichier.len() as u64).unwrap(),
            f.len() as u64,
            "étiquette finale {queue} mal retirée"
        );
    }
    // Un fichier plus court qu'une étiquette n'est pas lu hors de ses bornes.
    assert_eq!(fin_des_donnees(&mut Cursor::new(b"fLaC"), 4).unwrap(), 4);
}
