//! UDF (ECMA-167 / OSTA UDF 1.02 à 2.60), en lecture.
//!
//! Lu seulement quand l'image n'a pas d'arborescence ISO 9660 exploitable : la
//! plupart des DVD de données sont des images « pont », et leur ISO 9660 suffit.
//!
//! Chemin suivi : ancre (`AVDP`, secteur 256) → séquence de descripteurs de
//! volume → descripteur de partition (début de partition) et de volume logique
//! (taille de bloc, cartes de partition, emplacement du `FSD`) → descripteur
//! d'ensemble de fichiers → ICB de la racine → entrées de fichier et
//! identifiants de fichier.
//!
//! Partitions lues : physique (type 1), « sparable » (DVD-RW, lue comme une
//! partition physique, sans table de réaffectation) et, depuis UDF 2.50, la
//! partition de MÉTADONNÉES (Blu-ray, DVD gravés en UDF 2.50 ou 2.60). Celle-ci
//! range les entrées de fichier et les répertoires dans un « fichier de
//! métadonnées » posé sur la partition physique : un bloc de la partition de
//! métadonnées est le bloc de même rang DANS ce fichier, dont les étendues
//! disent où il se trouve. Si le fichier principal est illisible, son miroir
//! est essayé.
//!
//! Hors périmètre, rendu comme « pas d'UDF lisible » : les partitions
//! virtuelles (`VAT`, disques gravés en écriture incrémentale), les étendues
//! non enregistrées et les descripteurs d'allocation chaînés.

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

/// Une carte de partition du volume logique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carte {
    /// Type 1, ou type 2 « sparable » : la partition elle-même.
    Physique { numero: u16 },
    /// Type 2 « métadonnées » (UDF 2.50+) : emplacements, dans la partition
    /// physique, du fichier de métadonnées et de son miroir.
    Metadonnees {
        numero: u16,
        fichier: u32,
        miroir: u32,
    },
    /// Toute autre carte (partition virtuelle…) : non suivie.
    Autre,
}

const IDENTIFIANT_SPARABLE: &[u8] = b"*UDF Sparable Part";
const IDENTIFIANT_METADONNEES: &[u8] = b"*UDF Metadata Partition";

/// Les cartes de partition, dans l'ordre : leur rang est la « référence de
/// partition » des adresses longues.
fn cartes(table: &[u8]) -> Vec<Carte> {
    let mut sortie = Vec::new();
    let mut i = 0usize;
    while i + 2 <= table.len() {
        let type_carte = table[i];
        let longueur = table[i + 1] as usize;
        if longueur < 2 || i + longueur > table.len() {
            break;
        }
        let c = &table[i..i + longueur];
        sortie.push(match type_carte {
            1 if longueur >= 6 => Carte::Physique {
                numero: u16_le(c, 4),
            },
            2 if longueur >= 64
                && c[5..5 + IDENTIFIANT_SPARABLE.len()] == *IDENTIFIANT_SPARABLE =>
            {
                Carte::Physique {
                    numero: u16_le(c, 38),
                }
            }
            2 if longueur >= 64
                && c[5..5 + IDENTIFIANT_METADONNEES.len()] == *IDENTIFIANT_METADONNEES =>
            {
                Carte::Metadonnees {
                    numero: u16_le(c, 38),
                    fichier: u32_le(c, 40),
                    miroir: u32_le(c, 44),
                }
            }
            _ => Carte::Autre,
        });
        i += longueur;
    }
    sortie
}

struct Volume {
    taille_bloc: u64,
    numero_partition: u16,
    debut_partition: u64,
    longueur_partition: u64,
    taille_image: u64,
    cartes: Vec<Carte>,
    /// Les étendues PHYSIQUES du fichier de métadonnées, dans l'ordre ; vide
    /// tant qu'il n'est pas lu, ou sans partition de métadonnées.
    metadonnees: Vec<Etendue>,
}

impl Volume {
    /// Les étendues, en octets dans l'image, de `longueur` octets à partir du
    /// bloc `lbn` de la partition de référence `reference`.
    fn etendues(&self, reference: u16, lbn: u32, longueur: u64) -> io::Result<Vec<Etendue>> {
        match self.cartes.get(reference as usize) {
            Some(Carte::Physique { numero }) if *numero == self.numero_partition => {
                let fin_partition = self.longueur_partition * self.taille_bloc;
                let dans = lbn as u64 * self.taille_bloc;
                if dans + longueur > fin_partition {
                    return Err(invalide("UDF : bloc hors de la partition"));
                }
                let debut = self.debut_partition * self.taille_bloc + dans;
                if debut + longueur > self.taille_image {
                    return Err(invalide("UDF : bloc hors de l'image"));
                }
                Ok(vec![Etendue { debut, longueur }])
            }
            Some(Carte::Metadonnees { numero, .. }) if *numero == self.numero_partition => {
                // Le bloc `lbn` est l'octet `lbn × taille de bloc` du fichier
                // de métadonnées : on suit ses étendues jusque-là.
                let mut cherche = lbn as u64 * self.taille_bloc;
                let mut restant = longueur;
                let mut sortie = Vec::new();
                for e in &self.metadonnees {
                    if restant == 0 {
                        break;
                    }
                    if cherche >= e.longueur {
                        cherche -= e.longueur;
                        continue;
                    }
                    let utile = (e.longueur - cherche).min(restant);
                    sortie.push(Etendue {
                        debut: e.debut + cherche,
                        longueur: utile,
                    });
                    restant -= utile;
                    cherche = 0;
                }
                if restant != 0 {
                    return Err(invalide("UDF : bloc hors du fichier de métadonnées"));
                }
                Ok(sortie)
            }
            _ => Err(invalide(format!(
                "UDF : référence de partition {reference} non suivie"
            ))),
        }
    }

    /// Un bloc logique, et sa position dans l'image.
    fn lire_bloc(
        &self,
        fichier: &mut File,
        reference: u16,
        lbn: u32,
    ) -> io::Result<(Vec<u8>, u64)> {
        let etendues = self.etendues(reference, lbn, self.taille_bloc)?;
        let position = etendues.first().map_or(0, |e| e.debut);
        Ok((lire_etendues(fichier, &etendues)?, position))
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
        Some((taille_bloc, fsd_lbn, fsd_ref, table_cartes)),
    ) = (partition, volume_logique)
    else {
        return Ok(None);
    };
    if taille_bloc != SECTEUR {
        return Ok(None);
    }
    let mut volume = Volume {
        taille_bloc,
        numero_partition,
        debut_partition,
        longueur_partition,
        taille_image,
        cartes: cartes(&table_cartes),
        metadonnees: Vec::new(),
    };
    // Le FSD doit être sur NOTRE partition, par une carte suivie.
    match volume.cartes.get(fsd_ref as usize) {
        Some(Carte::Physique { numero }) if *numero == numero_partition => {}
        Some(&Carte::Metadonnees {
            numero,
            fichier: principal,
            miroir,
        }) if numero == numero_partition => {
            let Some(etendues) = fichier_de_metadonnees(fichier, &volume, principal)
                .or_else(|| fichier_de_metadonnees(fichier, &volume, miroir))
            else {
                return Ok(None);
            };
            volume.metadonnees = etendues;
        }
        _ => return Ok(None),
    }

    let (fsd, _) = volume.lire_bloc(fichier, fsd_ref, fsd_lbn)?;
    if !tag_valide(&fsd, TAG_ENSEMBLE_DE_FICHIERS) {
        return Err(invalide("UDF : descripteur d'ensemble de fichiers absent"));
    }
    let racine = (u16_le(&fsd, 408), u32_le(&fsd, 404));

    let mut sortie = Vec::new();
    let mut vus = std::collections::HashSet::new();
    descendre(fichier, &volume, racine, "", 0, &mut vus, &mut sortie)?;
    Ok(Some(IndexImage {
        systeme: Systeme::Udf,
        fichiers: sortie,
    }))
}

/// Les étendues physiques du fichier de métadonnées dont l'entrée est au bloc
/// `lbn` de la partition PHYSIQUE, ou `None` s'il est illisible.
fn fichier_de_metadonnees(fichier: &mut File, volume: &Volume, lbn: u32) -> Option<Vec<Etendue>> {
    let reference = volume.cartes.iter().position(
        |c| matches!(c, Carte::Physique { numero } if *numero == volume.numero_partition),
    );
    // Sans carte physique explicite (cas d'UDF 2.50 sur une seule carte), la
    // partition physique est lue directement.
    let physique;
    let (volume, reference) = match reference {
        Some(r) => (volume, r as u16),
        None => {
            physique = Volume {
                cartes: vec![Carte::Physique {
                    numero: volume.numero_partition,
                }],
                metadonnees: Vec::new(),
                ..*volume
            };
            (&physique, 0)
        }
    };
    let entree = lire_entree(fichier, volume, reference, lbn).ok()?;
    matches!(
        entree.type_fichier,
        TYPE_FICHIER_METADONNEES | TYPE_FICHIER_METADONNEES_MIROIR
    )
    .then_some(entree.etendues)
}

const TYPE_FICHIER_DOSSIER: u8 = 4;
const TYPE_FICHIER_METADONNEES: u8 = 250;
const TYPE_FICHIER_METADONNEES_MIROIR: u8 = 251;

/// Une entrée de fichier décodée : type, étendues dans l'image.
struct Entree {
    type_fichier: u8,
    etendues: Vec<Etendue>,
}

impl Entree {
    fn dossier(&self) -> bool {
        self.type_fichier == TYPE_FICHIER_DOSSIER
    }
}

fn lire_entree(
    fichier: &mut File,
    volume: &Volume,
    reference: u16,
    lbn: u32,
) -> io::Result<Entree> {
    let (b, position) = volume.lire_bloc(fichier, reference, lbn)?;
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
        // short_ad : même partition que l'entrée elle-même.
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
                pousser(
                    &mut etendues,
                    &mut restant,
                    volume,
                    reference,
                    u32_le(c, 4),
                    longueur,
                )?;
            }
        }
        // long_ad : la partition est nommée par l'adresse.
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
                pousser(
                    &mut etendues,
                    &mut restant,
                    volume,
                    u16_le(c, 8),
                    u32_le(c, 4),
                    longueur,
                )?;
            }
        }
        // Données incorporées dans l'entrée elle-même.
        3 => {
            let longueur = (l_ad as u64).min(taille);
            etendues.push(Etendue {
                debut: position + ad as u64,
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
        type_fichier,
        etendues,
    })
}

fn pousser(
    etendues: &mut Vec<Etendue>,
    restant: &mut u64,
    volume: &Volume,
    reference: u16,
    lbn: u32,
    longueur: u64,
) -> io::Result<()> {
    let utile = longueur.min(*restant);
    if utile == 0 {
        return Ok(());
    }
    for e in volume.etendues(reference, lbn, utile)? {
        // Deux étendues contiguës n'en font qu'une.
        match etendues.last_mut() {
            Some(d) if d.debut + d.longueur == e.debut => d.longueur += e.longueur,
            _ => etendues.push(e),
        }
    }
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
    (reference, lbn): (u16, u32),
    prefixe: &str,
    profondeur: usize,
    vus: &mut std::collections::HashSet<(u16, u32)>,
    sortie: &mut Vec<FichierInterne>,
) -> io::Result<()> {
    if profondeur > PROFONDEUR_MAX || !vus.insert((reference, lbn)) {
        return Ok(());
    }
    let entree = lire_entree(fichier, volume, reference, lbn)?;
    if !entree.dossier() {
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
        // L'ICB est une adresse longue : bloc, puis référence de partition.
        let icb = (u16_le(d, 28), u32_le(d, 24));
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
            let _ = descendre(fichier, volume, icb, &chemin, profondeur + 1, vus, sortie);
            continue;
        }
        let Ok(e) = lire_entree(fichier, volume, icb.0, icb.1) else {
            continue;
        };
        if e.dossier() || e.type_fichier >= TYPE_FICHIER_METADONNEES {
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
