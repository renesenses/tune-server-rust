//! #8968 (PR 1) — l'amorce et le remplissage d'un AAC en conteneur MP4.
//!
//! Un encodeur AAC-LC rend toujours plus de trames qu'il n'en a reçu : une
//! AMORCE au début (2 112 chez Apple, 1 024 chez ffmpeg et la plupart des
//! autres), puis un REMPLISSAGE qui complète le dernier bloc de 1 024. Lues
//! telles quelles, deux pistes d'un album enchaîné sont séparées par
//! remplissage + amorce de silence, soit 50 ms et plus : le blanc de #8968.
//!
//! Le conteneur le dit de deux façons, et symphonia 0.6 n'en applique
//! aucune : son démultiplexeur MP4 lit `elst` sans s'en servir, sa lecture
//! des balises range `iTunSMPB` sans l'interpréter, et son décodeur AAC
//! ignore les `trim_start`/`trim_end` des paquets (seul le MP3 les honore).
//! Constaté au banc de la PR 0 (`orchestrator/banc_jointure_8968_tests.rs`).
//!
//! * `----:com.apple.iTunes:iTunSMPB` (iTunes, afconvert, Apple Music) :
//!   ` 00000000 AMORCE REMPLISSAGE UTILES …` en hexadécimal ;
//! * la liste d'édition `moov/trak/edts/elst` (ffmpeg, la plupart des
//!   muxeurs) : `media_time` = amorce, `segment_duration` = durée utile, en
//!   unités de l'échelle du film (`mvhd`).
//!
//! `iTunSMPB` l'emporte quand les deux sont là (c'est ce que fait iTunes).
//! Ce module LIT ces valeurs ; [`Rogneur`] les applique, trame par trame, au
//! PCM décodé (`audio::decode`). Seul l'AAC est concerné : un ALAC porte
//! parfois un `iTunSMPB` (sans amorce), qui n'a rien à retirer.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Taille au-delà de laquelle un `moov` n'est pas lu (un `moov` audio pèse
/// quelques kilo-octets à quelques méga-octets).
const MOOV_MAX: u64 = 64 * 1024 * 1024;
/// Une amorce plus longue n'est pas une amorce (une seconde à 192 kHz).
const AMORCE_MAX: u64 = 192_000;

/// Ce que le conteneur dit des trames à retirer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RognageAac {
    /// Trames d'amorce à retirer au début du décodage.
    pub amorce: u64,
    /// Trames utiles après l'amorce ; `None` : jusqu'au bout (aucun
    /// remplissage connu).
    pub utiles: Option<u64>,
    /// Incertitude sur `utiles`, en trames : nulle quand la longueur est
    /// donnée en trames (`iTunSMPB`, `elst` à l'échelle de la cadence),
    /// d'une graduation de l'échelle du film sinon (`mvhd` à 1 000 ou 600).
    pub marge: u64,
}

impl RognageAac {
    /// Ce que le contrôle d'intégrité (#2218) et la coupure connue (#8483)
    /// peuvent tenir pour promis : les trames utiles MOINS l'incertitude de
    /// l'arrondi. Sans cette marge, une longueur arrondie au-dessus de la
    /// vérité ferait croire à une trame perdue (`decodage_incomplet`).
    pub(crate) fn trames_annoncees(&self) -> Option<u64> {
        self.utiles.map(|u| u.saturating_sub(self.marge))
    }
}

/// Mémoire des lectures : un déplacement relit la piste, il ne doit pas
/// relire son `moov` (jusqu'à quelques Mio). Clé : chemin, taille, date.
type CleDeFichier = (std::path::PathBuf, u64, Option<std::time::SystemTime>, u32);
static MEMOIRE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<CleDeFichier, Option<RognageAac>>>,
> = std::sync::LazyLock::new(Default::default);
/// Au-delà, la mémoire est vidée (une bibliothèque lue piste après piste
/// n'a besoin que des dernières).
const MEMOIRE_MAX: usize = 512;

/// Lit l'amorce et la longueur utile d'un AAC en MP4, en trames à `cadence`.
/// `None` : rien de lisible, ou rien à retirer — le décodage reste celui
/// d'avant à l'octet près.
pub(crate) fn lire(chemin: &Path, cadence: u32) -> Option<RognageAac> {
    let meta = std::fs::metadata(chemin).ok()?;
    let cle = (
        chemin.to_path_buf(),
        meta.len(),
        meta.modified().ok(),
        cadence,
    );
    if let Ok(m) = MEMOIRE.lock()
        && let Some(r) = m.get(&cle)
    {
        return *r;
    }
    let r = lire_sans_memoire(chemin, cadence);
    if let Ok(mut m) = MEMOIRE.lock() {
        if m.len() >= MEMOIRE_MAX {
            m.clear();
        }
        m.insert(cle, r);
    }
    r
}

fn lire_sans_memoire(chemin: &Path, cadence: u32) -> Option<RognageAac> {
    let moov = lire_moov(chemin)?;
    let r = depuis_moov(&moov, cadence)?;
    (r.amorce > 0 || r.utiles.is_some()).then_some(r)
}

/// #8968 — la durée JOUÉE d'un AAC en MP4, en millisecondes, quand le
/// conteneur dit son amorce et son remplissage : c'est celle que la
/// bibliothèque doit afficher (lofty lit `mdhd`, amorce et remplissage
/// compris : 6,06 s pour une piste de 6 s).
pub(crate) fn duree_jouee_ms(chemin: &Path, cadence: u32) -> Option<u64> {
    let utiles = lire(chemin, cadence)?.utiles?;
    (cadence > 0).then(|| (utiles * 1000 + u64::from(cadence) / 2) / u64::from(cadence))
}

/// Le contenu de la boîte `moov` (sans son en-tête), où qu'elle soit dans le
/// fichier.
fn lire_moov(chemin: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(chemin).ok()?;
    let longueur = f.metadata().ok()?.len();
    let mut pos = 0u64;
    while pos + 8 <= longueur {
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut tete = [0u8; 16];
        f.read_exact(&mut tete[..8]).ok()?;
        let mut taille = u64::from(u32::from_be_bytes(tete[..4].try_into().ok()?));
        let mut entete = 8u64;
        if taille == 1 {
            f.read_exact(&mut tete[8..16]).ok()?;
            taille = u64::from_be_bytes(tete[8..16].try_into().ok()?);
            entete = 16;
        } else if taille == 0 {
            taille = longueur - pos;
        }
        if taille < entete {
            return None;
        }
        if &tete[4..8] == b"moov" {
            let n = taille - entete;
            if n > MOOV_MAX {
                return None;
            }
            let mut v = vec![0u8; n as usize];
            f.read_exact(&mut v).ok()?;
            return Some(v);
        }
        pos = pos.checked_add(taille)?;
    }
    None
}

/// Les boîtes filles d'un contenu : `(type, contenu)`.
fn boites(contenu: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut v = Vec::new();
    let mut i = 0usize;
    while i + 8 <= contenu.len() {
        let mut taille = u32::from_be_bytes(contenu[i..i + 4].try_into().unwrap()) as usize;
        let mut entete = 8;
        if taille == 1 {
            if i + 16 > contenu.len() {
                break;
            }
            taille = u64::from_be_bytes(contenu[i + 8..i + 16].try_into().unwrap()) as usize;
            entete = 16;
        } else if taille == 0 {
            taille = contenu.len() - i;
        }
        if taille < entete || i + taille > contenu.len() {
            break;
        }
        v.push((&contenu[i + 4..i + 8], &contenu[i + entete..i + taille]));
        i += taille;
    }
    v
}

fn fille<'a>(contenu: &'a [u8], nom: &[u8; 4]) -> Option<&'a [u8]> {
    boites(contenu)
        .into_iter()
        .find(|(t, _)| *t == nom)
        .map(|(_, c)| c)
}

fn be32(o: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(o.get(i..i + 4)?.try_into().ok()?))
}

fn be64(o: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_be_bytes(o.get(i..i + 8)?.try_into().ok()?))
}

/// Échelle de temps d'un `mvhd` ou d'un `mdhd` (même disposition).
fn echelle(boite_pleine: &[u8]) -> Option<u32> {
    match boite_pleine.first()? {
        0 => be32(boite_pleine, 12),
        1 => be32(boite_pleine, 20),
        _ => None,
    }
    .filter(|e| *e > 0)
}

fn depuis_moov(moov: &[u8], cadence: u32) -> Option<RognageAac> {
    if cadence == 0 {
        return None;
    }
    let son = piste_son(moov)?;
    // `iTunSMPB` compte en trames de SORTIE à l'échelle de `mdhd`. Un
    // HE-AAC (SBR implicite) annonce le double de la cadence que symphonia
    // 0.6 décode (le cœur LC, sans SBR) : appliquer ces nombres tels quels
    // couperait deux fois trop. Cadences différentes : pas d'`iTunSMPB`,
    // seule la liste d'édition (convertie d'échelle) sert.
    if son.echelle == cadence
        && let Some(r) = itunsmpb(moov)
    {
        return Some(r);
    }
    elst(moov, &son, cadence)
}

/// La piste son du `moov` : sa boîte `trak`, l'échelle de son `mdhd` et sa
/// durée dans cette échelle (0 : inconnue, MP4 fragmenté).
struct PisteSon<'a> {
    trak: &'a [u8],
    echelle: u32,
    duree: u64,
}

fn piste_son(moov: &[u8]) -> Option<PisteSon<'_>> {
    for (t, trak) in boites(moov) {
        if t != b"trak" {
            continue;
        }
        let Some(mdia) = fille(trak, b"mdia") else {
            continue;
        };
        if fille(mdia, b"hdlr").and_then(|h| h.get(8..12)) != Some(b"soun".as_slice()) {
            continue;
        }
        let mdhd = fille(mdia, b"mdhd")?;
        let duree = match mdhd.first()? {
            0 => u64::from(be32(mdhd, 16)?),
            1 => be64(mdhd, 24)?,
            _ => return None,
        };
        return Some(PisteSon {
            trak,
            echelle: echelle(mdhd)?,
            duree,
        });
    }
    None
}

/// `----:com.apple.iTunes:iTunSMPB`, dans `moov/udta/meta/ilst` ou
/// `moov/meta/ilst`.
fn itunsmpb(moov: &[u8]) -> Option<RognageAac> {
    let metas = [
        fille(moov, b"udta").and_then(|u| fille(u, b"meta")),
        fille(moov, b"meta"),
    ];
    for meta in metas.into_iter().flatten() {
        // `meta` est une boîte pleine : 4 octets de version et drapeaux…
        // sauf chez QuickTime, qui les omet. Essayer les deux.
        for saut in [4usize, 0] {
            let Some(ilst) = meta.get(saut..).and_then(|m| fille(m, b"ilst")) else {
                continue;
            };
            for (t, libre) in boites(ilst) {
                if t != b"----" {
                    continue;
                }
                let filles = boites(libre);
                let nom = filles
                    .iter()
                    .find(|(t, _)| *t == b"name")
                    .and_then(|(_, c)| c.get(4..));
                if nom != Some(b"iTunSMPB".as_slice()) {
                    continue;
                }
                let valeur = filles
                    .iter()
                    .find(|(t, _)| *t == b"data")
                    .and_then(|(_, c)| c.get(8..))?;
                return lire_itunsmpb(std::str::from_utf8(valeur).ok()?);
            }
        }
    }
    None
}

/// ` 00000000 00000840 00000228 0000000000040998 …` : réservé, amorce,
/// remplissage, trames utiles.
fn lire_itunsmpb(texte: &str) -> Option<RognageAac> {
    let champs: Vec<u64> = texte
        .split_whitespace()
        .take(4)
        .map(|c| u64::from_str_radix(c, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    if champs.len() < 4 {
        return None;
    }
    let amorce = champs[1];
    if amorce > AMORCE_MAX {
        return None;
    }
    // Tout à zéro (certains muxeurs écrivent la balise sans la remplir) :
    // elle ne dit rien, la liste d'édition décide.
    if amorce == 0 && champs[2] == 0 && champs[3] == 0 {
        return None;
    }
    Some(RognageAac {
        amorce,
        utiles: Some(champs[3]).filter(|u| *u > 0),
        marge: 0,
    })
}

/// La liste d'édition de la piste son : première édition non vide.
fn elst(moov: &[u8], son: &PisteSon<'_>, cadence: u32) -> Option<RognageAac> {
    let echelle_film = echelle(fille(moov, b"mvhd")?)?;
    let liste = fille(fille(son.trak, b"edts")?, b"elst")?;
    let version = *liste.first()?;
    let n = be32(liste, 4)? as usize;
    let largeur = if version == 1 { 20 } else { 12 };
    let vers_trames = |v: u64, echelle: u32| -> u64 {
        ((u128::from(v) * u128::from(cadence) + u128::from(echelle) / 2) / u128::from(echelle))
            as u64
    };
    for k in 0..n.min(16) {
        let i = 8 + k * largeur;
        let (duree, debut) = if version == 1 {
            (be64(liste, i)?, be64(liste, i + 8)? as i64)
        } else {
            (
                u64::from(be32(liste, i)?),
                i64::from(be32(liste, i + 4)? as i32),
            )
        };
        if debut < 0 {
            // Édition vide (un blanc voulu en tête de piste vidéo) : on ne
            // fabrique pas de silence, on passe.
            continue;
        }
        let amorce = vers_trames(debut as u64, son.echelle);
        if amorce > AMORCE_MAX {
            return None;
        }
        let exacte = (u128::from(duree) * u128::from(cadence)) % u128::from(echelle_film) == 0;
        // Jamais plus que ce que la piste porte après l'amorce : un arrondi
        // au-dessus ne promet pas une trame qui n'existe pas.
        let total = (son.duree > 0).then(|| vers_trames(son.duree, son.echelle));
        let utiles = (duree > 0).then(|| {
            let u = vers_trames(duree, echelle_film);
            total.map_or(u, |t| u.min(t.saturating_sub(amorce)))
        });
        return Some(RognageAac {
            amorce,
            utiles,
            marge: if exacte {
                0
            } else {
                u64::from(cadence).div_ceil(u64::from(echelle_film))
            },
        });
    }
    None
}

/// Applique un [`RognageAac`] au PCM décodé, paquet après paquet.
#[derive(Debug, Clone)]
pub(crate) struct Rogneur {
    a_jeter: u64,
    restantes: Option<u64>,
}

impl Rogneur {
    /// Lecture depuis le début de la piste.
    pub(crate) fn depuis_le_debut(r: &RognageAac) -> Self {
        Self {
            a_jeter: r.amorce,
            restantes: r.utiles,
        }
    }

    /// Lecture qui commence `deja` trames UTILES après le début : la
    /// recherche a déjà enjambé l'amorce (voir [`Self::horodatage_de`]), il
    /// ne reste qu'à borner la fin.
    pub(crate) fn apres_recherche(r: &RognageAac, deja: u64) -> Self {
        Self {
            a_jeter: 0,
            restantes: r.utiles.map(|u| u.saturating_sub(deja)),
        }
    }

    /// L'horodatage (en trames du conteneur) de la trame utile `trame`.
    pub(crate) fn horodatage_de(r: &RognageAac, trame: u64) -> u64 {
        trame + r.amorce
    }

    /// Les échantillons à garder d'un paquet de `trames` trames entrelacées
    /// sur `canaux` canaux.
    pub(crate) fn garder(&mut self, trames: usize, canaux: usize) -> std::ops::Range<usize> {
        let saut = self.a_jeter.min(trames as u64);
        self.a_jeter -= saut;
        let reste = trames as u64 - saut;
        let garde = self.restantes.map_or(reste, |r| r.min(reste));
        if let Some(r) = self.restantes.as_mut() {
            *r -= garde;
        }
        let debut = saut as usize * canaux;
        debut..debut + garde as usize * canaux
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(nom: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(nom)
    }

    #[test]
    fn lit_l_itunsmpb_d_apple_8968() {
        assert_eq!(
            lire(&fixture("aac320.m4a"), 44_100),
            Some(RognageAac {
                amorce: 2_112,
                utiles: Some(264_600),
                marge: 0
            })
        );
    }

    #[test]
    fn lit_la_liste_d_edition_de_ffmpeg_8968() {
        assert_eq!(
            lire(&fixture("test.m4a"), 44_100),
            Some(RognageAac {
                amorce: 1_024,
                utiles: Some(44_100),
                marge: 0
            })
        );
    }

    #[test]
    fn un_fichier_sans_moov_ne_rogne_rien_8968() {
        assert_eq!(lire(&fixture("test.flac"), 44_100), None);
    }

    #[test]
    fn le_rogneur_retire_l_amorce_et_borne_la_fin_8968() {
        let mut r = Rogneur::depuis_le_debut(&RognageAac {
            amorce: 1_500,
            utiles: Some(2_000),
            marge: 0,
        });
        assert_eq!(r.garder(1_024, 2), 2_048..2_048);
        assert_eq!(r.garder(1_024, 2), 952..2_048);
        assert_eq!(r.garder(1_024, 2), 0..2_048);
        assert_eq!(r.garder(1_024, 2), 0..(2_000 - 548 - 1_024) * 2);
        assert_eq!(r.garder(1_024, 2), 0..0);
    }

    /// Un `moov` minimal : `mvhd`, une piste son (`mdhd`, `hdlr`, `elst`
    /// facultative) et un `iTunSMPB` facultatif.
    fn moov(
        echelle_film: u32,
        echelle_son: u32,
        duree_son: u32,
        edition: Option<(u32, i32)>,
        smpb: Option<&str>,
    ) -> Vec<u8> {
        fn boite(nom: &[u8; 4], contenu: &[u8]) -> Vec<u8> {
            let mut b = (contenu.len() as u32 + 8).to_be_bytes().to_vec();
            b.extend_from_slice(nom);
            b.extend_from_slice(contenu);
            b
        }
        let mut mvhd = vec![0u8; 12];
        mvhd.extend_from_slice(&echelle_film.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 4]);
        let mut mdhd = vec![0u8; 12];
        mdhd.extend_from_slice(&echelle_son.to_be_bytes());
        mdhd.extend_from_slice(&duree_son.to_be_bytes());
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"soun");
        let mdia = [boite(b"mdhd", &mdhd), boite(b"hdlr", &hdlr)].concat();
        let mut trak = Vec::new();
        if let Some((duree, debut)) = edition {
            let mut elst = vec![0u8; 4];
            elst.extend_from_slice(&1u32.to_be_bytes());
            elst.extend_from_slice(&duree.to_be_bytes());
            elst.extend_from_slice(&debut.to_be_bytes());
            elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            trak.extend(boite(b"edts", &boite(b"elst", &elst)));
        }
        trak.extend(boite(b"mdia", &mdia));
        let mut m = [boite(b"mvhd", &mvhd), boite(b"trak", &trak)].concat();
        if let Some(v) = smpb {
            let mut nom = vec![0u8; 4];
            nom.extend_from_slice(b"iTunSMPB");
            let mut donnees = vec![0u8; 8];
            donnees.extend_from_slice(v.as_bytes());
            let libre = [boite(b"name", &nom), boite(b"data", &donnees)].concat();
            let mut meta = vec![0u8; 4];
            meta.extend(boite(b"ilst", &boite(b"----", &libre)));
            m.extend(boite(b"udta", &boite(b"meta", &meta)));
        }
        m
    }

    const SMPB_APPLE: &str = " 00000000 00000840 00000228 0000000000040998";

    /// HE-AAC (SBR implicite) : `mdhd` à 44,1 kHz, mais symphonia 0.6 rend le
    /// cœur LC à 22,05 kHz. Les nombres d'`iTunSMPB`, comptés à 44,1 kHz, ne
    /// s'appliquent pas tels quels : ignorés.
    #[test]
    fn un_he_aac_n_applique_pas_itunsmpb_a_la_cadence_du_coeur_8968() {
        let m = moov(44_100, 44_100, 267_264, None, Some(SMPB_APPLE));
        assert_eq!(
            depuis_moov(&m, 44_100),
            Some(RognageAac {
                amorce: 2_112,
                utiles: Some(264_600),
                marge: 0
            }),
            "témoin : à cadence égale, la balise s'applique"
        );
        assert_eq!(depuis_moov(&m, 22_050), None);
    }

    /// HE-AAC avec liste d'édition : elle reste appliquée, convertie de
    /// l'échelle de `mdhd` à la cadence décodée.
    #[test]
    fn un_he_aac_garde_la_liste_d_edition_convertie_8968() {
        let m = moov(
            44_100,
            44_100,
            267_264,
            Some((264_600, 2_112)),
            Some(SMPB_APPLE),
        );
        assert_eq!(
            depuis_moov(&m, 22_050),
            Some(RognageAac {
                amorce: 1_056,
                utiles: Some(132_300),
                marge: 0
            })
        );
    }

    #[test]
    fn un_itunsmpb_tout_a_zero_laisse_decider_la_liste_d_edition_8968() {
        let zero = " 00000000 00000000 00000000 0000000000000000";
        let m = moov(44_100, 44_100, 45_124, Some((44_100, 1_024)), Some(zero));
        assert_eq!(
            depuis_moov(&m, 44_100),
            Some(RognageAac {
                amorce: 1_024,
                utiles: Some(44_100),
                marge: 0
            })
        );
        let seul = moov(44_100, 44_100, 45_124, None, Some(zero));
        assert_eq!(depuis_moov(&seul, 44_100), None);
    }

    /// `mvhd` à 600 (QuickTime) : la durée utile est arrondie, la marge le
    /// dit, et elle ne dépasse jamais ce que la piste porte après l'amorce.
    #[test]
    fn une_duree_d_edition_arrondie_porte_sa_marge_et_sa_borne_8968() {
        // 1,0004 s à 600 graduations par seconde : 600,24 → 600 ; et une
        // piste dont mdhd ne porte que 1 024 + 44 000 trames.
        let m = moov(600, 44_100, 45_024, Some((601, 1_024)), None);
        let r = depuis_moov(&m, 44_100).unwrap();
        assert_eq!(r.amorce, 1_024);
        assert_eq!(r.utiles, Some(44_000), "bornée à mdhd − amorce");
        assert_eq!(r.marge, 74, "une graduation de 1/600 s = 73,5 trames");
        assert_eq!(r.trames_annoncees(), Some(43_926));
    }

    /// La bibliothèque affiche la durée JOUÉE : 6 000 ms pour `aac320.m4a`
    /// (lofty lit 6 060 dans `mdhd`, amorce et remplissage compris), 1 000
    /// pour `test.m4a` (`elst`). Copie, pour ne pas toucher la fixture.
    #[test]
    fn la_bibliotheque_affiche_la_duree_jouee_8968() {
        let bac = crate::test_scratch::scratch_dir("amorce-aac-duree-8968");
        for (nom, attendue) in [("aac320.m4a", 6_000), ("test.m4a", 1_000)] {
            let chemin = bac.join(nom);
            std::fs::copy(fixture(nom), &chemin).unwrap();
            // Sans titre : le chemin `tagless_fallback`.
            let meta = crate::metadata::read_metadata(&chemin).expect("métadonnées lues");
            assert_eq!(meta.duration_ms, Some(attendue), "{nom} sans balise");
            // Avec un titre : le chemin des balises.
            intituler(&chemin);
            let meta = crate::metadata::read_metadata(&chemin).expect("métadonnées lues");
            assert_eq!(
                meta.title.as_deref(),
                Some("Banc 8968"),
                "{nom} : balise écrite"
            );
            assert_eq!(meta.duration_ms, Some(attendue), "{nom} balisé");
        }
    }

    fn intituler(chemin: &Path) {
        use lofty::config::{ParseOptions, WriteOptions};
        use lofty::file::AudioFile;
        use lofty::mp4::{Ilst, Mp4File};
        use lofty::tag::Accessor;
        let mut fh = File::open(chemin).unwrap();
        let mut m4a = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
        drop(fh);
        let mut ilst = m4a.ilst().cloned().unwrap_or_else(Ilst::default);
        ilst.set_title("Banc 8968".into());
        m4a.set_ilst(ilst);
        m4a.save_to_path(chemin, WriteOptions::default()).unwrap();
    }
}
