//! Épreuves du reste de #5299 : images multisession et UDF 2.50 à partition
//! de métadonnées, sur des images fabriquées ici (aucun outil externe).

use super::fabrique::{self, Contenu, Noms};
use super::*;
use std::io::Read;

fn motif(n: usize, graine: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32 * 13 + graine as u32) as u8 ^ (i >> 9) as u8)
        .collect()
}

fn ecrire(dossier: &Path, nom: &str, octets: &[u8]) -> PathBuf {
    let p = dossier.join(nom);
    std::fs::write(&p, octets).unwrap();
    p
}

/// Chaque fichier de `contenu` est dans l'index et se relit octet pour octet.
fn tout_se_relit(image: &Path, contenu: &Contenu) -> IndexImage {
    let index = lire_index(image).unwrap();
    for (chemin, octets) in contenu {
        let f = index.trouver(chemin).unwrap_or_else(|| {
            panic!(
                "{chemin} absent de l'index ; présents : {:?}",
                index.fichiers.iter().map(|f| &f.chemin).collect::<Vec<_>>()
            )
        });
        assert_eq!(f.taille(), octets.len() as u64, "{chemin} : taille");
        let mut lu = Vec::new();
        ouvrir_dans(image, chemin)
            .unwrap()
            .read_to_end(&mut lu)
            .unwrap();
        assert_eq!(&lu, octets, "{chemin} : octets");
    }
    index
}

fn premiere_gravure() -> Contenu {
    vec![
        ("Album A/01 - Première gravure.flac".into(), motif(7000, 1)),
        ("Album A/cover.jpg".into(), b"\xFF\xD8\xFF\xE0JPEG".to_vec()),
    ]
}

fn seconde_gravure() -> Contenu {
    vec![
        ("Album B/01 - Seconde gravure.mp3".into(), motif(5000, 2)),
        // Hors de Latin-1 : nom UCS-2 en Joliet comme en UDF (CS0 16 bits).
        ("Album B/02 - Œuvre encore.flac".into(), motif(9000, 3)),
    ]
}

fn les_deux() -> Contenu {
    premiere_gravure()
        .into_iter()
        .chain(seconde_gravure())
        .collect()
}

/// Un CD-R gravé en deux fois : le secteur 16 porte la première session, la
/// seconde commence après l'écart de session d'un CD (11 400 secteurs). La
/// vue du disque est celle de la DERNIÈRE session : les fichiers des deux
/// gravures, ceux de la première lus à leurs étendues d'origine.
#[test]
fn multisession_la_derniere_session_donne_tout_le_disque_5299() {
    let d = tempfile::tempdir().unwrap();
    let octets = fabrique::multisession(
        &premiere_gravure(),
        &seconde_gravure(),
        Noms::Joliet,
        11_400,
    );
    let image = ecrire(d.path(), "multisession.iso", &octets);
    let index = tout_se_relit(&image, &les_deux());
    assert_eq!(index.systeme, Systeme::Joliet);
    assert_eq!(index.fichiers.len(), 4);
    let contenu = contenu_audio(&image).unwrap();
    assert_eq!(contenu.pistes.len(), 3, "{:?}", contenu.pistes);
}

/// Sessions accolées (image prolongée sans recopie du secteur 16), noms
/// Rock Ridge.
#[test]
fn multisession_sans_ecart_en_rock_ridge_5299() {
    let d = tempfile::tempdir().unwrap();
    let octets =
        fabrique::multisession(&premiere_gravure(), &seconde_gravure(), Noms::RockRidge, 0);
    let image = ecrire(d.path(), "accolees.iso", &octets);
    let index = tout_se_relit(&image, &les_deux());
    assert_eq!(index.systeme, Systeme::RockRidge);
}

/// Une session suivante illisible (arborescence hors de l'image) ne fait pas
/// perdre la précédente.
#[test]
fn une_session_suivante_abimee_laisse_la_precedente_5299() {
    let d = tempfile::tempdir().unwrap();
    let mut octets =
        fabrique::multisession(&premiere_gravure(), &seconde_gravure(), Noms::Nus, 300);
    // La racine de la seconde session pointe au-delà de l'image.
    let s1_fin = fabrique::session_iso(&premiere_gravure(), Noms::Nus, 0, &[]).fin;
    let pvd = (s1_fin + 300 + 16) * 2048;
    assert_eq!(&octets[pvd..pvd + 7], b"\x01CD001\x01");
    octets[pvd + 156 + 2..pvd + 156 + 6].copy_from_slice(&u32::MAX.to_le_bytes());
    let image = ecrire(d.path(), "seconde_abimee.iso", &octets);
    let index = lire_index(&image).unwrap();
    assert_eq!(index.fichiers.len(), 2, "la première session reste lue");
    assert!(index.trouver("ALBUM_A/01___PRE.FLA").is_some());
}

/// Après la fin déclarée du volume, un secteur qui ressemble à un descripteur
/// primaire mais déclare un volume qui ne le dépasse pas n'ouvre pas de
/// session : l'image d'une seule session est lue comme avant.
#[test]
fn une_signature_isolee_n_ouvre_pas_de_session_5299() {
    let d = tempfile::tempdir().unwrap();
    let mut octets = fabrique::iso(&premiere_gravure(), Noms::Joliet);
    let fin = octets.len() / 2048;
    octets.resize((fin + 40) * 2048, 0);
    let faux = (fin + 20) * 2048;
    octets[faux..faux + 7].copy_from_slice(b"\x01CD001\x01");
    octets[faux + 80..faux + 84].copy_from_slice(&3u32.to_le_bytes());
    let image = ecrire(d.path(), "signature.iso", &octets);
    let index = tout_se_relit(&image, &premiere_gravure());
    assert_eq!(index.fichiers.len(), 2);
}

/// UDF 2.50 (Blu-ray) : entrées et répertoires dans le fichier de
/// métadonnées, rangé en deux étendues disjointes ; données des fichiers sur
/// la partition physique.
#[test]
fn udf_250_partition_de_metadonnees_5299() {
    let d = tempfile::tempdir().unwrap();
    let contenu = les_deux();
    let image = ecrire(d.path(), "bluray.iso", &fabrique::udf_250(&contenu, false));
    let index = tout_se_relit(&image, &contenu);
    assert_eq!(index.systeme, Systeme::Udf);
    assert_eq!(index.fichiers.len(), 4);
    let audio = contenu_pour_le_parcours(&image).expect("l'audio de l'image est indexé");
    assert_eq!(audio.pistes.len(), 3);
}

/// Le fichier de métadonnées principal illisible : son miroir est lu.
#[test]
fn udf_250_le_miroir_prend_le_relais_5299() {
    let d = tempfile::tempdir().unwrap();
    let contenu = les_deux();
    let image = ecrire(d.path(), "miroir.iso", &fabrique::udf_250(&contenu, true));
    tout_se_relit(&image, &contenu);
}

/// La fabrique UDF 1.02 reste lue (aucune régression de la partition
/// physique seule).
#[test]
fn udf_102_reste_lu_5299() {
    let d = tempfile::tempdir().unwrap();
    let contenu = les_deux();
    let image = ecrire(d.path(), "udf102.iso", &fabrique::udf(&contenu));
    tout_se_relit(&image, &contenu);
}
