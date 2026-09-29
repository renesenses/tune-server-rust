//! Les fichiers audio rangés DANS une image ISO de données (#5299).
//!
//! Une image de données — un DVD ou un CD gravé de FLAC, de MP3 — n'est pas un
//! SACD : elle porte un système de fichiers, ISO 9660, souvent doublé d'une
//! arborescence Joliet (noms longs Windows) ou d'entrées Rock Ridge (noms longs
//! POSIX). Jusqu'ici le parcours l'écartait en bloc sous la clé
//! [`super::iso_sacd::CLE_RAPPORT_ISO_DONNEES`]. Ce module lit ce système de
//! fichiers **sans monter l'image** et rend chaque fichier interne lisible par
//! une lecture CIBLÉE : seuls les secteurs demandés sont lus, jamais l'image
//! entière.
//!
//! ## Le chemin virtuel
//!
//! Un fichier interne est désigné par `image.iso!/dossier/fichier.flac` : le
//! chemin réel de l'image, le séparateur [`SEPARATEUR_INTERNE`], puis le chemin
//! dans l'image, toujours écrit avec des `/`. C'est ce chemin qui entre en base
//! dans `tracks.file_path` : chaque piste interne est un fichier entier, pas
//! une tranche — elle suit donc la règle des pistes CUE « qui occupent le
//! fichier entier » (`cue_bibliotheque::piste_en_ligne`), qui gardent leur
//! `file_path` et restent trouvables par chemin, et non celle des tranches
//! (`cue_media_path` + `cue_start_ms`), qui n'a pas de sens ici.
//!
//! `Path::extension()` d'un chemin virtuel rend celle du fichier interne, et
//! `Path::parent()` le dossier interne : le regroupement par dossier, les
//! règles d'extension et le rapport de scan le traitent donc comme un fichier
//! ordinaire rangé dans un dossier `image.iso!`.
//!
//! ## Pourquoi un lecteur maison
//!
//! Les crates examinées le 27/09/2026 : `cdfs` (MIT/Apache) tire `fuser`,
//! `clap` et `nom` — un client FUSE entier pour lire un répertoire ;
//! `iso9660` (MIT/Apache) n'a pas bougé depuis 2023 et ignore Joliet ;
//! `hadris-iso` (MIT) est vivante mais apporte sept caisses internes et une
//! API qui change à chaque version majeure ; `udf` est sous « Apache-2.0 OR
//! GPL-2.0 ». Ce que Tune demande est étroit — lister une arborescence et lire
//! des étendues — et tient ici sans aucune dépendance nouvelle.
//!
//! ## Ce qui est lu
//!
//! - ISO 9660 (ECMA-119), fichiers en plusieurs étendues compris (> 4 Gio) ;
//! - Joliet (descripteur supplémentaire, noms UCS-2) ;
//! - Rock Ridge (entrées `NM` du SUSP, zones de continuation `CE` comprises),
//!   préféré à Joliet quand les deux existent : ses noms ne sont pas bornés à
//!   64 caractères ;
//! - UDF 1.02 à 2.01 sur partition physique, quand l'image n'a PAS
//!   d'arborescence ISO 9660 exploitable (voir [`udf`]).
//!
//! Tout est borné : profondeur, nombre d'entrées, étendues contenues dans
//! l'image. Une image forgée ou abîmée rend une erreur, jamais une boucle.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Ce qui sépare le chemin de l'image du chemin interne.
pub const SEPARATEUR_INTERNE: &str = "!/";

/// Taille d'un secteur logique de CD/DVD, en octets.
const SECTEUR: u64 = 2048;

/// Premier secteur des descripteurs de volume ISO 9660.
const PREMIER_DESCRIPTEUR: u64 = 16;

/// Au-delà, la suite des descripteurs est tenue pour abîmée.
const MAX_DESCRIPTEURS: u64 = 64;

/// Profondeur maximale d'arborescence parcourue.
const PROFONDEUR_MAX: usize = 32;

/// Nombre maximal d'entrées indexées dans une image.
const ENTREES_MAX: usize = 200_000;

/// Taille maximale d'un répertoire lu d'un bloc (16 Mio).
const REPERTOIRE_MAX: u64 = 16 * 1024 * 1024;

/// Nombre d'index d'images gardés en mémoire.
const CACHE_MAX: usize = 16;

/// Un morceau contigu d'un fichier interne, en octets dans l'image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Etendue {
    pub debut: u64,
    pub longueur: u64,
}

/// Un fichier (jamais un dossier) de l'image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FichierInterne {
    /// Chemin dans l'image, sans `/` initial, séparé par des `/`.
    pub chemin: String,
    pub etendues: Vec<Etendue>,
}

impl FichierInterne {
    pub fn taille(&self) -> u64 {
        self.etendues.iter().map(|e| e.longueur).sum()
    }
}

/// Le système de fichiers d'où provient l'index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Systeme {
    Iso9660,
    Joliet,
    RockRidge,
    Udf,
}

/// L'arborescence d'une image, réduite à ses fichiers.
#[derive(Debug, Clone)]
pub struct IndexImage {
    pub systeme: Systeme,
    /// Triés par chemin : l'ordre du parcours de bibliothèque est stable.
    pub fichiers: Vec<FichierInterne>,
}

impl IndexImage {
    /// Le fichier de ce chemin interne, casse exacte d'abord, puis sans tenir
    /// compte de la casse (un nom ISO 9660 nu est en majuscules).
    pub fn trouver(&self, interne: &str) -> Option<&FichierInterne> {
        let interne = interne.trim_start_matches('/');
        self.fichiers
            .iter()
            .find(|f| f.chemin == interne)
            .or_else(|| {
                self.fichiers
                    .iter()
                    .find(|f| f.chemin.eq_ignore_ascii_case(interne))
            })
    }
}

fn invalide(motif: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, motif.into())
}

/// Dit si ce chemin porte l'extension `.iso`, sans tenir compte de la casse.
pub fn est_extension_iso(chemin: &Path) -> bool {
    chemin
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// Coupe un chemin virtuel en (image, chemin interne), ou `None` pour un
/// chemin ordinaire.
///
/// Le séparateur n'est reconnu que juste après une extension `.iso` : un
/// dossier nommé `Wow!` ne fait pas d'un fichier un chemin virtuel. Sous
/// Windows, un `\` peut suivre le `!` si le chemin a été renormalisé.
pub fn decouper(chemin: &str) -> Option<(PathBuf, String)> {
    let minuscules = chemin.to_ascii_lowercase();
    let mut depart = 0;
    while let Some(i) = minuscules[depart..].find(".iso!") {
        let fin_image = depart + i + ".iso".len();
        let apres = &chemin[fin_image + 1..];
        if let Some(reste) = apres.strip_prefix('/').or_else(|| apres.strip_prefix('\\')) {
            let interne = reste.replace('\\', "/");
            if !interne.is_empty() {
                return Some((PathBuf::from(&chemin[..fin_image]), interne));
            }
        }
        depart = fin_image;
    }
    None
}

/// Le chemin virtuel d'un fichier interne.
pub fn chemin_virtuel(image: &Path, interne: &str) -> String {
    format!(
        "{}{}{}",
        image.to_string_lossy(),
        SEPARATEUR_INTERNE,
        interne.trim_start_matches('/')
    )
}

/// Dit si ce chemin désigne un fichier DANS une image.
pub fn est_chemin_virtuel(chemin: &str) -> bool {
    decouper(chemin).is_some()
}

// ─── Lecture des structures ─────────────────────────────────────────────

fn lire_a(fichier: &mut File, position: u64, tampon: &mut [u8]) -> io::Result<()> {
    fichier.seek(SeekFrom::Start(position))?;
    fichier.read_exact(tampon)
}

fn u16_le(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_le(b: &[u8], i: usize) -> u64 {
    let mut o = [0u8; 8];
    o.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(o)
}

/// Un enregistrement de répertoire ISO 9660, décodé.
#[derive(Debug, Clone)]
struct Enregistrement {
    extent: u32,
    longueur: u32,
    attributs_etendus: u8,
    drapeaux: u8,
    nom_brut: Vec<u8>,
    usage_systeme: Vec<u8>,
}

impl Enregistrement {
    fn est_dossier(&self) -> bool {
        self.drapeaux & 0x02 != 0
    }
    /// Le fichier continue dans l'enregistrement suivant (fichier > 4 Gio).
    fn a_une_suite(&self) -> bool {
        self.drapeaux & 0x80 != 0
    }
    fn est_point_ou_point_point(&self) -> bool {
        self.nom_brut == [0] || self.nom_brut == [1]
    }
}

/// Découpe les enregistrements d'un répertoire lu d'un bloc.
fn enregistrements(donnees: &[u8]) -> Vec<Enregistrement> {
    let mut sortie = Vec::new();
    let mut i = 0usize;
    while i < donnees.len() {
        let longueur = donnees[i] as usize;
        if longueur == 0 {
            // Un enregistrement ne franchit jamais une frontière de secteur :
            // un octet nul veut dire « la suite est au secteur suivant ».
            let suivant = (i / SECTEUR as usize + 1) * SECTEUR as usize;
            if suivant <= i {
                break;
            }
            i = suivant;
            continue;
        }
        if longueur < 34 || i + longueur > donnees.len() {
            break;
        }
        let r = &donnees[i..i + longueur];
        let longueur_nom = r[32] as usize;
        if 33 + longueur_nom > longueur {
            break;
        }
        let nom_brut = r[33..33 + longueur_nom].to_vec();
        // Octet de bourrage quand la longueur du nom est paire.
        let debut_su = 33 + longueur_nom + usize::from(longueur_nom % 2 == 0);
        let usage_systeme = if debut_su < longueur {
            r[debut_su..].to_vec()
        } else {
            Vec::new()
        };
        sortie.push(Enregistrement {
            extent: u32_le(r, 2),
            longueur: u32_le(r, 10),
            attributs_etendus: r[1],
            drapeaux: r[25],
            nom_brut,
            usage_systeme,
        });
        i += longueur;
    }
    sortie
}

/// Le nom ISO 9660 nu : sans `;1`, sans point final orphelin.
fn nom_iso(brut: &[u8]) -> String {
    let texte = String::from_utf8_lossy(brut);
    let sans_version = texte.split(';').next().unwrap_or("");
    sans_version.trim_end_matches('.').to_string()
}

/// Le nom Joliet : UCS-2 grand-boutiste, même nettoyage.
fn nom_joliet(brut: &[u8]) -> String {
    let unites: Vec<u16> = brut
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    let texte = String::from_utf16_lossy(&unites);
    let sans_version = texte.split(';').next().unwrap_or("");
    sans_version.trim_end_matches('.').to_string()
}

/// Le nom Rock Ridge (`NM`) porté par une zone d'usage système, s'il y en a.
///
/// `saut` est le nombre d'octets à ignorer en tête (entrée `SP` de la racine).
/// Les zones de continuation (`CE`) sont suivies, au plus huit fois.
fn nom_rock_ridge(
    fichier: &mut File,
    zone: &[u8],
    saut: usize,
    taille_image: u64,
) -> Option<String> {
    let mut nom: Vec<u8> = Vec::new();
    let mut trouve = false;
    let mut courante: Vec<u8> = zone.get(saut..).unwrap_or(&[]).to_vec();
    for _ in 0..8 {
        let mut continuation: Option<(u64, u64, u64)> = None;
        let mut i = 0usize;
        while i + 4 <= courante.len() {
            let sig = &courante[i..i + 2];
            let longueur = courante[i + 2] as usize;
            if longueur < 4 || i + longueur > courante.len() {
                break;
            }
            let e = &courante[i..i + longueur];
            match sig {
                b"NM" if longueur >= 5 => {
                    let drapeaux = e[4];
                    // CURRENT (.) ou PARENT (..) : pas un nom.
                    if drapeaux & 0x06 == 0 {
                        nom.extend_from_slice(&e[5..]);
                        trouve = true;
                    }
                }
                b"CE" if longueur >= 28 => {
                    continuation = Some((
                        u32_le(e, 4) as u64,
                        u32_le(e, 12) as u64,
                        u32_le(e, 20) as u64,
                    ));
                }
                b"ST" => break,
                _ => {}
            }
            i += longueur;
        }
        let Some((bloc, decalage, longueur)) = continuation else {
            break;
        };
        let debut = bloc * SECTEUR + decalage;
        if longueur == 0 || longueur > SECTEUR || debut + longueur > taille_image {
            break;
        }
        let mut suite = vec![0u8; longueur as usize];
        if lire_a(fichier, debut, &mut suite).is_err() {
            break;
        }
        courante = suite;
    }
    (trouve && !nom.is_empty()).then(|| String::from_utf8_lossy(&nom).into_owned())
}

/// Le décalage `SP` de Rock Ridge, lu sur l'entrée `.` de la racine.
fn saut_susp_racine(racine_point: &Enregistrement) -> Option<usize> {
    let su = &racine_point.usage_systeme;
    (su.len() >= 7 && &su[0..2] == b"SP" && su[4] == 0xBE && su[5] == 0xEF).then(|| su[6] as usize)
}

/// Les deux arborescences candidates : primaire et Joliet.
struct Descripteurs {
    primaire: Option<[u8; 34]>,
    joliet: Option<[u8; 34]>,
}

fn lire_descripteurs(fichier: &mut File) -> io::Result<Descripteurs> {
    let mut d = Descripteurs {
        primaire: None,
        joliet: None,
    };
    let mut secteur = [0u8; SECTEUR as usize];
    for n in 0..MAX_DESCRIPTEURS {
        if lire_a(fichier, (PREMIER_DESCRIPTEUR + n) * SECTEUR, &mut secteur).is_err() {
            break;
        }
        if &secteur[1..6] != b"CD001" {
            break;
        }
        let mut racine = [0u8; 34];
        racine.copy_from_slice(&secteur[156..190]);
        match secteur[0] {
            1 if d.primaire.is_none() => d.primaire = Some(racine),
            2 => {
                // Séquences d'échappement Joliet : %/@, %/C, %/E (niveaux 1-3).
                let esc = &secteur[88..91];
                if esc[0] == b'%' && esc[1] == b'/' && matches!(esc[2], b'@' | b'C' | b'E') {
                    d.joliet = Some(racine);
                }
            }
            255 => break,
            _ => {}
        }
    }
    Ok(d)
}

struct Parcours<'a> {
    fichier: &'a mut File,
    taille_image: u64,
    joliet: bool,
    saut_rr: Option<usize>,
    vus: std::collections::HashSet<u32>,
    sortie: Vec<FichierInterne>,
}

impl Parcours<'_> {
    fn nom(&mut self, e: &Enregistrement) -> String {
        if let Some(saut) = self.saut_rr
            && let Some(nom) =
                nom_rock_ridge(self.fichier, &e.usage_systeme, saut, self.taille_image)
        {
            return nom;
        }
        if self.joliet {
            nom_joliet(&e.nom_brut)
        } else {
            nom_iso(&e.nom_brut)
        }
    }

    fn lire_repertoire(&mut self, extent: u32, longueur: u32) -> io::Result<Vec<Enregistrement>> {
        let debut = extent as u64 * SECTEUR;
        let longueur = longueur as u64;
        if longueur == 0 || longueur > REPERTOIRE_MAX || debut + longueur > self.taille_image {
            return Err(invalide("répertoire hors de l'image"));
        }
        let mut donnees = vec![0u8; longueur as usize];
        lire_a(self.fichier, debut, &mut donnees)?;
        Ok(enregistrements(&donnees))
    }

    fn descendre(
        &mut self,
        extent: u32,
        longueur: u32,
        prefixe: &str,
        profondeur: usize,
    ) -> io::Result<()> {
        if profondeur > PROFONDEUR_MAX || !self.vus.insert(extent) {
            return Ok(());
        }
        let entrees = self.lire_repertoire(extent, longueur)?;
        let mut i = 0;
        while i < entrees.len() {
            let e = entrees[i].clone();
            i += 1;
            if e.est_point_ou_point_point() {
                continue;
            }
            let nom = self.nom(&e);
            if nom.is_empty() || nom == "." || nom == ".." || nom.contains('/') {
                continue;
            }
            let chemin = if prefixe.is_empty() {
                nom
            } else {
                format!("{prefixe}/{nom}")
            };
            if e.est_dossier() {
                // Un sous-dossier illisible n'emporte pas le reste de l'image.
                let _ = self.descendre(e.extent, e.longueur, &chemin, profondeur + 1);
                continue;
            }
            let mut etendues = vec![self.etendue(&e)?];
            let mut courant = e;
            // Fichier en plusieurs étendues : les enregistrements suivants
            // portent le même nom et le drapeau « suite » sur tous sauf le
            // dernier.
            while courant.a_une_suite() && i < entrees.len() {
                courant = entrees[i].clone();
                i += 1;
                etendues.push(self.etendue(&courant)?);
            }
            if self.sortie.len() >= ENTREES_MAX {
                return Err(invalide("image : trop d'entrées"));
            }
            self.sortie.push(FichierInterne { chemin, etendues });
        }
        Ok(())
    }

    fn etendue(&self, e: &Enregistrement) -> io::Result<Etendue> {
        let debut = (e.extent as u64 + e.attributs_etendus as u64) * SECTEUR;
        let longueur = e.longueur as u64;
        if debut + longueur > self.taille_image {
            return Err(invalide("fichier hors de l'image"));
        }
        Ok(Etendue { debut, longueur })
    }
}

/// Lit l'arborescence ISO 9660 (Rock Ridge, sinon Joliet, sinon noms nus).
fn index_iso9660(fichier: &mut File, taille_image: u64) -> io::Result<Option<IndexImage>> {
    let d = lire_descripteurs(fichier)?;
    let Some(primaire) = d.primaire else {
        return Ok(None);
    };
    let racine_pvd = &enregistrements(&primaire)[..];
    let Some(racine_pvd) = racine_pvd.first().cloned() else {
        return Err(invalide("racine ISO 9660 illisible"));
    };

    // Rock Ridge se reconnaît à l'entrée `SP` de l'enregistrement `.` de la
    // racine — il faut donc lire le premier secteur de celle-ci.
    let mut saut_rr = None;
    {
        let mut p = Parcours {
            fichier,
            taille_image,
            joliet: false,
            saut_rr: None,
            vus: Default::default(),
            sortie: Vec::new(),
        };
        if let Ok(entrees) =
            p.lire_repertoire(racine_pvd.extent, racine_pvd.longueur.min(SECTEUR as u32))
            && let Some(point) = entrees.first()
        {
            saut_rr = saut_susp_racine(point);
        }
    }

    let (racine, joliet, systeme) = match (saut_rr, d.joliet) {
        (Some(_), _) => (racine_pvd, false, Systeme::RockRidge),
        (None, Some(j)) => match enregistrements(&j).first().cloned() {
            Some(r) => (r, true, Systeme::Joliet),
            None => (racine_pvd, false, Systeme::Iso9660),
        },
        (None, None) => (racine_pvd, false, Systeme::Iso9660),
    };
    let mut p = Parcours {
        fichier,
        taille_image,
        joliet,
        saut_rr,
        vus: Default::default(),
        sortie: Vec::new(),
    };
    p.descendre(racine.extent, racine.longueur, "", 0)?;
    Ok(Some(IndexImage {
        systeme,
        fichiers: p.sortie,
    }))
}

/// Lit l'index d'une image : ISO 9660 d'abord, UDF sinon.
///
/// Une image « pont » (DVD vidéo, la plupart des DVD de données) porte les
/// deux : ISO 9660 suffit alors, et ses noms Joliet ou Rock Ridge sont complets.
/// Une image dont l'arborescence ISO 9660 est vide mais qui porte UDF (disque
/// gravé en UDF seul, ou volume ISO de façade) est lue par UDF.
pub fn lire_index(image: &Path) -> io::Result<IndexImage> {
    let mut fichier = File::open(image)?;
    let taille_image = fichier.metadata()?.len();
    let iso = index_iso9660(&mut fichier, taille_image)?;
    if let Some(index) = &iso
        && !index.fichiers.is_empty()
    {
        let mut index = index.clone();
        index.fichiers.sort_by(|a, b| a.chemin.cmp(&b.chemin));
        return Ok(index);
    }
    if let Some(mut index) = udf::lire(&mut fichier, taille_image)? {
        index.fichiers.sort_by(|a, b| a.chemin.cmp(&b.chemin));
        return Ok(index);
    }
    match iso {
        Some(index) => Ok(index),
        None => Err(invalide("ni ISO 9660 ni UDF")),
    }
}

// ─── Cache des index ────────────────────────────────────────────────────

type CleCache = (PathBuf, u64, Option<std::time::SystemTime>);

fn cache() -> &'static Mutex<Vec<(CleCache, Arc<IndexImage>)>> {
    static CACHE: OnceLock<Mutex<Vec<(CleCache, Arc<IndexImage>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// L'index de l'image, relu seulement si elle a changé (taille ou date).
///
/// Le scan lit les balises de chaque piste, puis la lecture ouvre chaque piste :
/// sans ce cache, chaque ouverture relirait toute l'arborescence de l'image.
pub fn index(image: &Path) -> io::Result<Arc<IndexImage>> {
    let meta = std::fs::metadata(image)?;
    let cle: CleCache = (image.to_path_buf(), meta.len(), meta.modified().ok());
    if let Ok(c) = cache().lock()
        && let Some((_, index)) = c.iter().find(|(k, _)| *k == cle)
    {
        return Ok(index.clone());
    }
    let index = Arc::new(lire_index(image)?);
    if let Ok(mut c) = cache().lock() {
        c.retain(|(k, _)| k.0 != cle.0);
        if c.len() >= CACHE_MAX {
            c.remove(0);
        }
        c.push((cle, index.clone()));
    }
    Ok(index)
}

// ─── Lecture d'un fichier interne ───────────────────────────────────────

/// Un fichier interne ouvert : `Read + Seek` sur ses seules étendues.
///
/// Chaque lecture se traduit par un `seek` dans l'image au décalage de
/// l'étendue, puis une lecture bornée à celle-ci : jamais un octet hors du
/// fichier, jamais l'image entière.
#[derive(Debug)]
pub struct LecteurInterne {
    image: File,
    etendues: Vec<Etendue>,
    taille: u64,
    position: u64,
    /// Position physique courante du descripteur d'image, pour éviter un
    /// `seek` par lecture quand la lecture est séquentielle.
    physique: Option<u64>,
}

impl LecteurInterne {
    pub fn nouveau(image: File, fichier: &FichierInterne) -> Self {
        Self {
            image,
            etendues: fichier.etendues.clone(),
            taille: fichier.taille(),
            position: 0,
            physique: None,
        }
    }

    pub fn taille(&self) -> u64 {
        self.taille
    }
}

impl Read for LecteurInterne {
    fn read(&mut self, tampon: &mut [u8]) -> io::Result<usize> {
        if tampon.is_empty() || self.position >= self.taille {
            return Ok(0);
        }
        let mut logique = 0u64;
        for e in &self.etendues {
            if self.position < logique + e.longueur {
                let dans = self.position - logique;
                let reste = (e.longueur - dans) as usize;
                let a_lire = reste.min(tampon.len());
                let cible = e.debut + dans;
                if self.physique != Some(cible) {
                    self.image.seek(SeekFrom::Start(cible))?;
                }
                let n = self.image.read(&mut tampon[..a_lire])?;
                self.position += n as u64;
                self.physique = Some(cible + n as u64);
                return Ok(n);
            }
            logique += e.longueur;
        }
        Ok(0)
    }
}

impl Seek for LecteurInterne {
    fn seek(&mut self, depuis: SeekFrom) -> io::Result<u64> {
        let cible: i128 = match depuis {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => self.taille as i128 + d as i128,
            SeekFrom::Current(d) => self.position as i128 + d as i128,
        };
        if cible < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "déplacement avant le début du fichier",
            ));
        }
        self.position = cible as u64;
        Ok(self.position)
    }
}

/// Ouvre le fichier désigné par un chemin virtuel.
pub fn ouvrir(chemin: &str) -> io::Result<LecteurInterne> {
    let (image, interne) =
        decouper(chemin).ok_or_else(|| invalide("pas un chemin dans une image"))?;
    ouvrir_dans(&image, &interne)
}

/// Ouvre `interne` dans `image`.
pub fn ouvrir_dans(image: &Path, interne: &str) -> io::Result<LecteurInterne> {
    let index = index(image)?;
    let fichier = index.trouver(interne).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{interne} : absent de l'image {}", image.display()),
        )
    })?;
    Ok(LecteurInterne::nouveau(File::open(image)?, fichier))
}

/// Taille et date d'un fichier interne : sa taille propre, la date de l'image.
///
/// Une image ne change pas sans que sa date change : c'est elle qui dit au scan
/// incrémental qu'il faut relire les pistes qu'elle contient.
pub fn stat(chemin: &str) -> Option<(u64, Option<std::time::SystemTime>)> {
    let (image, interne) = decouper(chemin)?;
    let meta = std::fs::metadata(&image).ok()?;
    let index = index(&image).ok()?;
    let fichier = index.trouver(&interne)?;
    Some((fichier.taille(), meta.modified().ok()))
}

/// Lit un fichier interne entier, borné à `plafond` octets (pochettes).
pub fn lire_entier(chemin: &str, plafond: u64) -> io::Result<Vec<u8>> {
    let mut lecteur = ouvrir(chemin)?;
    if lecteur.taille() > plafond {
        return Err(invalide("fichier interne trop gros"));
    }
    let mut sortie = Vec::with_capacity(lecteur.taille() as usize);
    lecteur.read_to_end(&mut sortie)?;
    Ok(sortie)
}

// ─── Ce que le parcours de bibliothèque en retient ──────────────────────

/// Les extensions qu'une image peut livrer au catalogue.
///
/// Ce sont les formats que `decode.rs` sait lire depuis un `Read + Seek` :
/// ceux qu'il confie à symphonia, et — depuis que leurs lecteurs propres
/// ouvrent le fichier par [`ouvrir_fichier`] et non plus par `File::open` —
/// AIFF, DSF, DFF, APE, WavPack, Opus et Matroska. Tout autre format de
/// `LIBRARY_AUDIO_EXTENSIONS` trouvé dans une image serait compté et nommé
/// dans le rapport, sans devenir une piste impossible à jouer.
pub const EXTENSIONS_AUDIO_DANS_IMAGE: &[&str] = &[
    "flac", "mp3", "m4a", "alac", "ogg", "oga", "wav", "aiff", "aif", "aifc", "dsf", "dff", "ape",
    "wv", "opus", "mkv", "mka", "webm", "weba",
];

/// Clé de rapport des fichiers audio d'une image dont le format n'est pas
/// livrable depuis l'image (voir [`EXTENSIONS_AUDIO_DANS_IMAGE`]).
pub const CLE_RAPPORT_FORMAT_DANS_IMAGE: &str = "iso-format-non-lu";

/// Motif rendu à l'utilisateur pour ces fichiers.
pub const MOTIF_FORMAT_DANS_IMAGE: &str =
    "fichier audio dans une image ISO : ce format ne se lit pas depuis l'image";

/// Les noms de pochette reconnus dans une image, par ordre de préférence.
pub const NOMS_DE_POCHETTE: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "front.jpg",
    "front.png",
];

fn extension_minuscule(chemin: &str) -> Option<String> {
    let nom = chemin.rsplit('/').next()?;
    let (_, ext) = nom.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// Ce que le parcours tire d'une image de données.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContenuAudio {
    /// Chemins virtuels des pistes admises.
    pub pistes: Vec<PathBuf>,
    /// Fichiers audio présents mais d'un format que l'image ne sait pas livrer.
    pub ecartes: Vec<String>,
}

/// Les pistes audio d'une image de données.
pub fn contenu_audio(image: &Path) -> io::Result<ContenuAudio> {
    let index = index(image)?;
    let mut contenu = ContenuAudio::default();
    for f in &index.fichiers {
        let Some(ext) = extension_minuscule(&f.chemin) else {
            continue;
        };
        let nom = f.chemin.rsplit('/').next().unwrap_or("");
        if nom.starts_with("._") {
            continue;
        }
        if EXTENSIONS_AUDIO_DANS_IMAGE.contains(&ext.as_str()) {
            contenu
                .pistes
                .push(PathBuf::from(chemin_virtuel(image, &f.chemin)));
        } else if crate::audio::support::LIBRARY_AUDIO_EXTENSIONS.contains(&ext.as_str())
            && ext != "iso"
        {
            contenu.ecartes.push(chemin_virtuel(image, &f.chemin));
        }
    }
    Ok(contenu)
}

/// Ce que le parcours de bibliothèque retient d'une image de données, ou
/// `None` quand elle ne porte aucun fichier audio — ou qu'elle est illisible :
/// elle reste alors écartée et NOMMÉE comme avant, sous
/// [`super::iso_sacd::CLE_RAPPORT_ISO_DONNEES`].
pub fn contenu_pour_le_parcours(image: &Path) -> Option<ContenuAudio> {
    match contenu_audio(image) {
        Ok(c) if !c.pistes.is_empty() || !c.ecartes.is_empty() => {
            tracing::info!(
                image = %image.display(),
                pistes = c.pistes.len(),
                ecartes = c.ecartes.len(),
                "iso_donnees_audio_indexee"
            );
            Some(c)
        }
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(image = %image.display(), error = %e, "iso_donnees_illisible");
            None
        }
    }
}

/// La pochette du dossier interne qui contient ce chemin virtuel, s'il y en a
/// une : `cover.jpg`, `folder.jpg`… du même dossier interne, puis de chacun
/// de ses parents jusqu'à la racine de l'image. Une image porte d'ordinaire UN
/// album : la pochette d'un coffret posée au-dessus de `CD1/` et `CD2/` vaut
/// pour les deux. Rend le chemin VIRTUEL de l'image trouvée.
pub fn chemin_de_pochette(chemin: &Path) -> Option<PathBuf> {
    let (image, interne) = decouper(&chemin.to_string_lossy())?;
    let index = index(&image).ok()?;
    let mut dossiers = Vec::new();
    let mut courant = interne.as_str();
    while let Some((parent, _)) = courant.rsplit_once('/') {
        dossiers.push(parent);
        courant = parent;
    }
    dossiers.push("");
    for d in dossiers {
        for nom in NOMS_DE_POCHETTE {
            let cible = if d.is_empty() {
                (*nom).to_string()
            } else {
                format!("{d}/{nom}")
            };
            if let Some(f) = index
                .fichiers
                .iter()
                .find(|f| f.chemin.eq_ignore_ascii_case(&cible))
            {
                return Some(PathBuf::from(chemin_virtuel(&image, &f.chemin)));
            }
        }
    }
    None
}

// ─── Les points d'entrée des consommateurs ──────────────────────────────
//
// Chaque consommateur d'un chemin de piste (balises, empreinte, pochette,
// lecture, parcours) garde son code pour un fichier ordinaire et appelle l'une
// de ces fonctions d'abord : elles rendent `None` pour un chemin ordinaire, ce
// qui laisse son comportement strictement inchangé.

/// Ouvre le fichier interne si `chemin` est virtuel ; `None` sinon.
pub fn ouvrir_si_virtuel(chemin: &Path) -> Option<io::Result<LecteurInterne>> {
    let texte = chemin.to_string_lossy();
    decouper(&texte).map(|(image, interne)| ouvrir_dans(&image, &interne))
}

/// Un fichier de piste ouvert, sur le disque ou dans une image : ce que les
/// lecteurs propres à un format (AIFF, DSF, DFF, APE, WavPack, Opus, Matroska)
/// ouvrent à la place d'un `File`, pour lire une image comme un dossier.
#[derive(Debug)]
pub enum FichierSource {
    Disque(File),
    Image(LecteurInterne),
}

impl FichierSource {
    /// La longueur du fichier (celle du fichier interne pour une image).
    pub fn longueur(&self) -> io::Result<u64> {
        match self {
            Self::Disque(f) => f.metadata().map(|m| m.len()),
            Self::Image(l) => Ok(l.taille()),
        }
    }
}

impl Read for FichierSource {
    fn read(&mut self, tampon: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Disque(f) => f.read(tampon),
            Self::Image(l) => l.read(tampon),
        }
    }
}

impl Seek for FichierSource {
    fn seek(&mut self, depuis: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Disque(f) => f.seek(depuis),
            Self::Image(l) => l.seek(depuis),
        }
    }
}

impl symphonia::core::io::MediaSource for FichierSource {
    fn is_seekable(&self) -> bool {
        match self {
            Self::Disque(f) => symphonia::core::io::MediaSource::is_seekable(f),
            Self::Image(_) => true,
        }
    }
    fn byte_len(&self) -> Option<u64> {
        match self {
            Self::Disque(f) => symphonia::core::io::MediaSource::byte_len(f),
            Self::Image(l) => Some(l.taille()),
        }
    }
}

/// Ouvre `chemin` : le fichier interne d'une image si le chemin est virtuel,
/// le fichier du disque sinon — `File::open` inchangé pour tout chemin
/// ordinaire.
pub fn ouvrir_fichier<P: AsRef<Path>>(chemin: P) -> io::Result<FichierSource> {
    let chemin = chemin.as_ref();
    match ouvrir_si_virtuel(chemin) {
        Some(lecteur) => lecteur.map(FichierSource::Image),
        None => File::open(chemin).map(FichierSource::Disque),
    }
}

/// Lit en entier (au plus 64 Mio) le fichier interne si `chemin` est
/// virtuel ; `None` sinon.
pub fn lire_si_virtuel(chemin: &Path) -> Option<io::Result<Vec<u8>>> {
    est_chemin_virtuel(&chemin.to_string_lossy())
        .then(|| lire_entier(&chemin.to_string_lossy(), 64 * 1024 * 1024))
}

/// Taille et date de modification (secondes depuis Epoch) d'un chemin,
/// ordinaire ou virtuel. `None` si le fichier n'existe pas.
///
/// Pour un fichier interne : sa taille propre, la date de l'IMAGE. Le scan
/// incrémental compare ce couple à celui qu'il a rangé en base ; une image
/// réécrite change de date, et toutes ses pistes sont relues.
pub fn taille_et_mtime(chemin: &Path) -> Option<(u64, f64)> {
    let secondes = |t: Option<std::time::SystemTime>| {
        t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    };
    let texte = chemin.to_string_lossy();
    if est_chemin_virtuel(&texte) {
        return stat(&texte).map(|(taille, date)| (taille, secondes(date)));
    }
    let meta = chemin.metadata().ok()?;
    Some((meta.len(), secondes(meta.modified().ok())))
}

impl symphonia::core::io::MediaSource for LecteurInterne {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.taille)
    }
}

/// La source symphonia du fichier interne si `chemin` est virtuel ; `None`
/// sinon — l'appelant ouvre alors son `File` comme avant.
pub fn source_symphonia(
    chemin: &str,
) -> Option<Result<Box<dyn symphonia::core::io::MediaSource>, String>> {
    est_chemin_virtuel(chemin).then(|| {
        ouvrir(chemin)
            .map(|l| Box::new(l) as Box<dyn symphonia::core::io::MediaSource>)
            .map_err(|e| format!("open (image ISO): {e}"))
    })
}

pub mod udf;

/// Fabrique d'images pour les épreuves, ici et dans `tune-server` — comme
/// `crate::test_scratch`, elle n'est appelée par aucun chemin de production.
#[doc(hidden)]
pub mod fabrique;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod epreuves_5299;

#[cfg(test)]
mod epreuves_formats_5299;
