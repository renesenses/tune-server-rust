//! UDF (ECMA-167 / OSTA UDF 1.02 à 2.01), en lecture, sur partition physique.
//!
//! Lu seulement quand l'image n'a pas d'arborescence ISO 9660 exploitable : la
//! plupart des DVD de données sont des images « pont », et leur ISO 9660 suffit.
//!
//! Chemin suivi : ancre (`AVDP`, secteur 256) → séquence de descripteurs de
//! volume → descripteur de partition (début de partition) et de volume logique
//! (taille de bloc, emplacement du `FSD`) → descripteur d'ensemble de fichiers
//! → ICB de la racine → entrées de fichier et identifiants de fichier.
//!
//! Hors périmètre, rendu comme « pas d'UDF lisible » : les partitions de
//! métadonnées (UDF 2.50, Blu-ray) et les partitions virtuelles (disques
//! multisessions en écriture incrémentale). Une partition « sparable »
//! (DVD-RW) est lue comme une partition physique, sans table de réaffectation.

use std::fs::File;
use std::io;

use super::{
    Etendue, FichierInterne, IndexImage, SECTEUR, Systeme, invalide, lire_a, u16_le, u32_le, u64_le,
};

const TAG_AVDP: u16 = 2;
const TAG_PARTITION: u16 = 5;
const TAG_VOLUME_LOGIQUE: u16 = 6;
const TAG_TERMINAISON: u16 = 8;
const TAG_ENSEMBLE_DE_FICHIERS: u16 = 256;
const TAG_IDENTIFIANT_DE_FICHIER: u16 = 257;
const TAG_ENTREE_DE_FICHIER: u16 = 261;
const TAG_ENTREE_DE_FICHIER_ETENDUE: u16 = 266;

const PROFONDEUR_MAX: usize = 32;
const ENTREES_MAX: usize = 200_000;
const REPERTOIRE_MAX: u64 = 16 * 1024 * 1024;

/// Le contrôle d'un en-tête de descripteur : somme des octets 0-15 hors 4.
fn tag_valide(b: &[u8], attendu: u16) -> bool {
    if b.len() < 16 || u16_le(b, 0) != attendu {
        return false;
    }
    let somme = b[..16]
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .fold(0u8, |acc, (_, v)| acc.wrapping_add(*v));
    somme == b[4]
}

struct Volume {
    taille_bloc: u64,
    debut_partition: u64,
    longueur_partition: u64,
    taille_image: u64,
}

impl Volume {
    fn octet(&self, lbn: u32) -> u64 {
        (self.debut_partition + lbn as u64) * self.taille_bloc
    }

    fn lire_bloc(&self, fichier: &mut File, lbn: u32) -> io::Result<Vec<u8>> {
        if lbn as u64 >= self.longueur_partition {
            return Err(invalide("UDF : bloc hors de la partition"));
        }
        let position = self.octet(lbn);
        if position + self.taille_bloc > self.taille_image {
            return Err(invalide("UDF : bloc hors de l'image"));
        }
        let mut b = vec![0u8; self.taille_bloc as usize];
        lire_a(fichier, position, &mut b)?;
        Ok(b)
    }
}

/// Lit l'index UDF, ou `None` si l'image n'en porte pas (ou pas un lisible).
pub(super) fn lire(fichier: &mut File, taille_image: u64) -> io::Result<Option<IndexImage>> {
    let mut ancre = vec![0u8; SECTEUR as usize];
    if lire_a(fichier, 256 * SECTEUR, &mut ancre).is_err() || !tag_valide(&ancre, TAG_AVDP) {
        return Ok(None);
    }
    let longueur_vds = u32_le(&ancre, 16) as u64;
    let debut_vds = u32_le(&ancre, 20) as u64;

    let mut partition: Option<(u16, u64, u64)> = None;
    let mut volume_logique: Option<(u64, u32, u16, Vec<u8>)> = None;
    let secteurs = (longueur_vds / SECTEUR).min(64);
    for n in 0..secteurs {
        let mut d = vec![0u8; SECTEUR as usize];
        if lire_a(fichier, (debut_vds + n) * SECTEUR, &mut d).is_err() {
            break;
        }
        let id = u16_le(&d, 0);
        if id == TAG_TERMINAISON && tag_valide(&d, id) {
            break;
        }
        if id == TAG_PARTITION && tag_valide(&d, id) && partition.is_none() {
            partition = Some((
                u16_le(&d, 22),
                u32_le(&d, 188) as u64,
                u32_le(&d, 192) as u64,
            ));
        }
        if id == TAG_VOLUME_LOGIQUE && tag_valide(&d, id) && volume_logique.is_none() {
            let taille_bloc = u32_le(&d, 212) as u64;
            let fsd_lbn = u32_le(&d, 252);
            let fsd_partition = u16_le(&d, 256);
            let longueur_cartes = (u32_le(&d, 264) as usize).min(d.len().saturating_sub(440));
            volume_logique = Some((
                taille_bloc,
                fsd_lbn,
                fsd_partition,
                d[440..440 + longueur_cartes].to_vec(),
            ));
        }
    }
    let (
        Some((numero_partition, debut_partition, longueur_partition)),
        Some((taille_bloc, fsd_lbn, fsd_ref, cartes)),
    ) = (partition, volume_logique)
    else {
        return Ok(None);
    };
    if taille_bloc != SECTEUR {
        return Ok(None);
    }
    // Seule une carte de type 1 (physique) vers NOTRE partition est suivie ;
    // les cartes de type 2 ne sont admises que « sparables ».
    if !carte_physique_ou_sparable(&cartes, fsd_ref, numero_partition) {
        return Ok(None);
    }
    let volume = Volume {
        taille_bloc,
        debut_partition,
        longueur_partition,
        taille_image,
    };

    let fsd = volume.lire_bloc(fichier, fsd_lbn)?;
    if !tag_valide(&fsd, TAG_ENSEMBLE_DE_FICHIERS) {
        return Err(invalide("UDF : descripteur d'ensemble de fichiers absent"));
    }
    let racine = u32_le(&fsd, 404);

    let mut sortie = Vec::new();
    let mut vus = std::collections::HashSet::new();
    descendre(fichier, &volume, racine, "", 0, &mut vus, &mut sortie)?;
    Ok(Some(IndexImage {
        systeme: Systeme::Udf,
        fichiers: sortie,
    }))
}

fn carte_physique_ou_sparable(cartes: &[u8], reference: u16, numero: u16) -> bool {
    let mut i = 0usize;
    let mut indice = 0u16;
    while i + 2 <= cartes.len() {
        let type_carte = cartes[i];
        let longueur = cartes[i + 1] as usize;
        if longueur < 2 || i + longueur > cartes.len() {
            return false;
        }
        if indice == reference {
            return match type_carte {
                1 if longueur >= 6 => u16_le(cartes, i + 4) == numero,
                2 if longueur >= 64 => {
                    &cartes[i + 5..i + 5 + 18] == b"*UDF Sparable Part"
                        && u16_le(cartes, i + 38) == numero
                }
                _ => false,
            };
        }
        indice += 1;
        i += longueur;
    }
    false
}

/// Une entrée de fichier décodée : type, taille, étendues.
struct Entree {
    dossier: bool,
    etendues: Vec<Etendue>,
}

fn lire_entree(fichier: &mut File, volume: &Volume, lbn: u32) -> io::Result<Entree> {
    let b = volume.lire_bloc(fichier, lbn)?;
    let id = u16_le(&b, 0);
    let (debut_ad, l_ea, l_ad) = if tag_valide(&b, TAG_ENTREE_DE_FICHIER) {
        (176usize, u32_le(&b, 168) as usize, u32_le(&b, 172) as usize)
    } else if tag_valide(&b, TAG_ENTREE_DE_FICHIER_ETENDUE) {
        (216usize, u32_le(&b, 208) as usize, u32_le(&b, 212) as usize)
    } else {
        return Err(invalide(format!(
            "UDF : entrée de fichier attendue, tag {id}"
        )));
    };
    let type_fichier = b[27];
    let drapeaux = u16_le(&b, 34);
    let taille = u64_le(&b, 56);
    let ad = debut_ad + l_ea;
    if ad + l_ad > b.len() {
        return Err(invalide("UDF : descripteurs d'allocation hors du bloc"));
    }
    let mut etendues = Vec::new();
    let mut restant = taille;
    match drapeaux & 0x07 {
        // short_ad
        0 => {
            for c in b[ad..ad + l_ad].chunks_exact(8) {
                let brut = u32_le(c, 0);
                let (genre, longueur) = (brut >> 30, (brut & 0x3FFF_FFFF) as u64);
                if longueur == 0 {
                    break;
                }
                if genre != 0 {
                    return Err(invalide("UDF : étendue non enregistrée ou chaînée"));
                }
                pousser(&mut etendues, &mut restant, volume, u32_le(c, 4), longueur)?;
            }
        }
        // long_ad
        1 => {
            for c in b[ad..ad + l_ad].chunks_exact(16) {
                let brut = u32_le(c, 0);
                let (genre, longueur) = (brut >> 30, (brut & 0x3FFF_FFFF) as u64);
                if longueur == 0 {
                    break;
                }
                if genre != 0 {
                    return Err(invalide("UDF : étendue non enregistrée ou chaînée"));
                }
                pousser(&mut etendues, &mut restant, volume, u32_le(c, 4), longueur)?;
            }
        }
        // Données incorporées dans l'entrée elle-même.
        3 => {
            let longueur = (l_ad as u64).min(taille);
            etendues.push(Etendue {
                debut: volume.octet(lbn) + ad as u64,
                longueur,
            });
            restant -= longueur;
        }
        _ => return Err(invalide("UDF : descripteurs d'allocation étendus")),
    }
    if restant != 0 {
        return Err(invalide("UDF : fichier plus court que sa taille annoncée"));
    }
    Ok(Entree {
        dossier: type_fichier == 4,
        etendues,
    })
}

fn pousser(
    etendues: &mut Vec<Etendue>,
    restant: &mut u64,
    volume: &Volume,
    lbn: u32,
    longueur: u64,
) -> io::Result<()> {
    let utile = longueur.min(*restant);
    if utile == 0 {
        return Ok(());
    }
    let debut = volume.octet(lbn);
    if debut + utile > volume.taille_image {
        return Err(invalide("UDF : étendue hors de l'image"));
    }
    etendues.push(Etendue {
        debut,
        longueur: utile,
    });
    *restant -= utile;
    Ok(())
}

fn lire_etendues(fichier: &mut File, etendues: &[Etendue]) -> io::Result<Vec<u8>> {
    let total: u64 = etendues.iter().map(|e| e.longueur).sum();
    if total > REPERTOIRE_MAX {
        return Err(invalide("UDF : répertoire trop grand"));
    }
    let mut sortie = Vec::with_capacity(total as usize);
    for e in etendues {
        let mut b = vec![0u8; e.longueur as usize];
        lire_a(fichier, e.debut, &mut b)?;
        sortie.extend_from_slice(&b);
    }
    Ok(sortie)
}

/// Un identifiant OSTA CS0 : 8 bits (Latin-1) ou 16 bits (UCS-2 grand-boutiste).
fn nom_cs0(b: &[u8]) -> String {
    let Some((&compression, reste)) = b.split_first() else {
        return String::new();
    };
    match compression {
        8 | 254 => reste.iter().map(|&c| c as char).collect(),
        16 | 255 => {
            let unites: Vec<u16> = reste
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&unites)
        }
        _ => String::new(),
    }
}

fn descendre(
    fichier: &mut File,
    volume: &Volume,
    lbn: u32,
    prefixe: &str,
    profondeur: usize,
    vus: &mut std::collections::HashSet<u32>,
    sortie: &mut Vec<FichierInterne>,
) -> io::Result<()> {
    if profondeur > PROFONDEUR_MAX || !vus.insert(lbn) {
        return Ok(());
    }
    let entree = lire_entree(fichier, volume, lbn)?;
    if !entree.dossier {
        return Err(invalide("UDF : la racine n'est pas un dossier"));
    }
    let donnees = lire_etendues(fichier, &entree.etendues)?;
    let mut i = 0usize;
    while i + 38 <= donnees.len() {
        let d = &donnees[i..];
        if !tag_valide(d, TAG_IDENTIFIANT_DE_FICHIER) {
            break;
        }
        let caracteristiques = d[18];
        let l_fi = d[19] as usize;
        let icb_lbn = u32_le(d, 24);
        let l_iu = u16_le(d, 36) as usize;
        let longueur = (38 + l_iu + l_fi).div_ceil(4) * 4;
        if 38 + l_iu + l_fi > d.len() {
            break;
        }
        i += longueur;
        // Supprimé, parent : ignorés.
        if caracteristiques & 0x04 != 0 || caracteristiques & 0x08 != 0 || l_fi == 0 {
            continue;
        }
        let nom = nom_cs0(&d[38 + l_iu..38 + l_iu + l_fi]);
        if nom.is_empty() || nom == "." || nom == ".." || nom.contains('/') {
            continue;
        }
        let chemin = if prefixe.is_empty() {
            nom
        } else {
            format!("{prefixe}/{nom}")
        };
        if caracteristiques & 0x02 != 0 {
            let _ = descendre(
                fichier,
                volume,
                icb_lbn,
                &chemin,
                profondeur + 1,
                vus,
                sortie,
            );
            continue;
        }
        let Ok(e) = lire_entree(fichier, volume, icb_lbn) else {
            continue;
        };
        if e.dossier {
            continue;
        }
        if sortie.len() >= ENTREES_MAX {
            return Err(invalide("UDF : trop d'entrées"));
        }
        sortie.push(FichierInterne {
            chemin,
            etendues: e.etendues,
        });
    }
    Ok(())
}
