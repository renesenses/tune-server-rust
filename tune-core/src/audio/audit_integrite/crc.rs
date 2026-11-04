//! Mode rapide de l'audit d'intégrité (#7847) : les sommes de contrôle des
//! conteneurs, sans décoder un seul échantillon.
//!
//! - FLAC (RFC 9639) : CRC-8 de chaque en-tête de trame et CRC-16 de chaque
//!   trame entière.
//! - Ogg (RFC 3533), donc Vorbis, Opus et FLAC-in-Ogg : CRC-32 de chaque page
//!   et continuité des numéros de page de chaque flux logique.
//!
//! Le parcours ne fait confiance à aucune longueur : la fin d'une trame FLAC
//! est l'en-tête valide (CRC-8 juste, paramètres du flux, numérotation qui
//! suit) de la trame suivante. Un motif de synchro `0xFFF8` présent dans les
//! données audio n'en est donc pas une frontière. Le fichier est lu par blocs
//! et le tampon ne garde que la trame en cours.
//!
//! Le MD5 de STREAMINFO n'est PAS vérifié ici : il exige le décodage, c'est le
//! rôle du mode complet. Réécriture d'après les spécifications.
use std::io::{self, Read};

const fn table_crc8() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}
const fn table_crc16() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}
const fn table_crc32() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}
static CRC8: [u8; 256] = table_crc8();
static CRC16: [u16; 256] = table_crc16();
static CRC32: [u32; 256] = table_crc32();

/// CRC-8 FLAC : polynôme 0x07, valeur initiale 0.
fn crc8(octets: &[u8]) -> u8 {
    octets.iter().fold(0, |crc, &o| CRC8[(crc ^ o) as usize])
}
/// CRC-16 FLAC : polynôme 0x8005, valeur initiale 0, sans réflexion. Celui
/// d'une trame suivie de son propre CRC (gros-boutiste) vaut 0.
fn crc16(crc: u16, octet: u8) -> u16 {
    (crc << 8) ^ CRC16[((crc >> 8) as u8 ^ octet) as usize]
}
/// CRC-32 Ogg : polynôme 0x04C11DB7, valeur initiale 0, sans réflexion.
fn crc32(crc: u32, octet: u8) -> u32 {
    (crc << 8) ^ CRC32[((crc >> 24) as u8 ^ octet) as usize]
}

/// Où se trouve une trame ou une page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Lieu {
    /// Rang dans le fichier, à partir de 0 (le numéro de trame FLAC en
    /// blocs fixes, l'ordre des pages Ogg).
    pub numero: u64,
    /// Position du premier octet de la trame ou de la page.
    pub octet: u64,
    /// Premier échantillon couvert (granule de la page précédente en Ogg),
    /// s'il est connu.
    pub echantillon: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Conteneur {
    Flac,
    Ogg,
}

#[derive(Debug, Clone)]
pub(super) struct Faute {
    pub lieu: Lieu,
    /// `crc_trame`, `trames_manquantes`, `entete_illisible` (FLAC) ;
    /// `crc_page`, `pages_manquantes`, `donnees_hors_page` (Ogg).
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub(super) struct Constat {
    pub conteneur: Conteneur,
    /// Trames ou pages contrôlées.
    pub unites: u64,
    pub fautes: u64,
    pub premiere_faute: Option<Faute>,
    /// Le fichier s'arrête avant sa fin annoncée : la trame ou la page où il
    /// est coupé. Distinct d'une faute de somme de contrôle.
    pub tronque: Option<Lieu>,
    /// Début de chaque trame ou page reconnue, pour les témoins.
    #[cfg(test)]
    pub debuts: Vec<u64>,
}

impl Constat {
    fn new(conteneur: Conteneur) -> Self {
        Self {
            conteneur,
            unites: 0,
            fautes: 0,
            premiere_faute: None,
            tronque: None,
            #[cfg(test)]
            debuts: Vec::new(),
        }
    }
    fn faute(&mut self, lieu: Lieu, code: &'static str, detail: String) {
        self.fautes += 1;
        if self.premiere_faute.is_none() {
            self.premiere_faute = Some(Faute { lieu, code, detail });
        }
    }
    fn noter_debut(&mut self, _octet: u64) {
        #[cfg(test)]
        self.debuts.push(_octet);
    }
}

#[derive(Debug)]
pub(super) enum Analyse {
    Constat(Constat),
    /// Ni FLAC ni Ogg : le mode rapide n'a rien à contrôler.
    NonApplicable,
    /// `continuer` a rendu faux : aucun verdict.
    Interrompu,
}

const BLOC: usize = 1 << 20;

/// Lecture par blocs avec une fenêtre glissante : `base` est la position dans
/// le fichier du premier octet du tampon.
struct Lecteur<R> {
    source: R,
    tampon: Vec<u8>,
    base: u64,
    epuise: bool,
}

impl<R: Read> Lecteur<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            tampon: Vec::new(),
            base: 0,
            epuise: false,
        }
    }
    /// Vrai si les octets `[pos, pos + n)` sont dans le fichier.
    fn disponible(&mut self, pos: u64, n: usize) -> io::Result<bool> {
        let fin = pos + n as u64;
        while (self.base + self.tampon.len() as u64) < fin {
            if self.epuise {
                return Ok(false);
            }
            let avant = self.tampon.len();
            self.tampon.resize(avant + BLOC, 0);
            let lus = loop {
                match self.source.read(&mut self.tampon[avant..]) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.tampon.truncate(avant);
                        return Err(e);
                    }
                }
            };
            self.tampon.truncate(avant + lus);
            self.epuise = lus == 0;
        }
        Ok(true)
    }
    fn octet(&self, pos: u64) -> u8 {
        self.tampon[(pos - self.base) as usize]
    }
    fn tranche(&self, pos: u64, n: usize) -> &[u8] {
        let debut = (pos - self.base) as usize;
        &self.tampon[debut..debut + n]
    }
    /// Jusqu'à `n` octets à partir de `pos`, moins en fin de fichier.
    fn au_plus(&mut self, pos: u64, n: usize) -> io::Result<&[u8]> {
        self.disponible(pos, n)?;
        let fin = (self.base + self.tampon.len() as u64).min(pos + n as u64);
        Ok(self.tranche(pos, fin.saturating_sub(pos) as usize))
    }
    /// Oublie ce qui précède `pos`, qui ne sera plus relu.
    fn oublier(&mut self, pos: u64) {
        let n = (pos - self.base) as usize;
        if n >= BLOC && n <= self.tampon.len() {
            self.tampon.drain(..n);
            self.base = pos;
        }
    }
}

/// Analyse rapide d'un FLAC ou d'un Ogg. `continuer` est appelé à chaque
/// trame ou page.
pub(super) fn analyser(
    source: impl Read,
    mut continuer: impl FnMut() -> bool,
) -> io::Result<Analyse> {
    let mut lecteur = Lecteur::new(source);
    let mut debut = 0u64;
    // Une étiquette ID3v2 précède parfois « fLaC ».
    if lecteur.disponible(0, 10)? && lecteur.tranche(0, 3) == b"ID3" {
        let taille = lecteur
            .tranche(6, 4)
            .iter()
            .fold(0u64, |t, &o| (t << 7) | u64::from(o & 0x7f));
        let pied = if lecteur.octet(5) & 0x10 != 0 { 10 } else { 0 };
        debut = 10 + taille + pied;
    }
    if !lecteur.disponible(debut, 4)? {
        return Ok(Analyse::NonApplicable);
    }
    match lecteur.tranche(debut, 4) {
        b"fLaC" => flac(&mut lecteur, debut + 4, &mut continuer),
        b"OggS" if debut == 0 => ogg(&mut lecteur, &mut continuer),
        _ => Ok(Analyse::NonApplicable),
    }
}

/// Ce que STREAMINFO dit du flux : de quoi rejeter un faux en-tête.
struct Flux {
    canaux: u8,
    bits: u8,
    frequence: u32,
    trame_min: u32,
    echantillons: u64,
}

struct Entete {
    longueur: usize,
    variable: bool,
    /// Numéro de trame (blocs fixes) ou d'échantillon (blocs variables).
    numero: u64,
    bloc: u64,
}

/// Lit un en-tête de trame FLAC ; `None` s'il n'en est pas un valide pour ce
/// flux (synchro, champs réservés, paramètres, CRC-8).
fn entete(o: &[u8], flux: &Flux) -> Option<Entete> {
    if o.len() < 6 || o[0] != 0xFF || o[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable = o[1] & 1 == 1;
    let (code_bloc, code_freq) = (o[2] >> 4, o[2] & 0x0f);
    let (code_canaux, code_bits) = (o[3] >> 4, (o[3] >> 1) & 7);
    if code_bloc == 0 || code_freq == 0x0f || code_canaux > 10 || code_bits == 3 || o[3] & 1 != 0 {
        return None;
    }
    let canaux = if code_canaux < 8 { code_canaux + 1 } else { 2 };
    let bits = match code_bits {
        0 => flux.bits,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        _ => 32,
    };
    if canaux != flux.canaux || bits != flux.bits {
        return None;
    }
    // Numéro codé à la manière d'UTF-8 : 6 octets au plus en blocs fixes,
    // 7 en blocs variables.
    let (mut numero, suite) = match o[4].leading_ones() {
        0 => (u64::from(o[4]), 0),
        n @ 2..=6 => (u64::from(o[4] & (0x7f >> n)), n as usize - 1),
        7 if variable => (0, 6),
        _ => return None,
    };
    let mut i = 5;
    for _ in 0..suite {
        let o = *o.get(i)?;
        if o & 0xC0 != 0x80 {
            return None;
        }
        numero = (numero << 6) | u64::from(o & 0x3f);
        i += 1;
    }
    let bloc = match code_bloc {
        1 => 192,
        2..=5 => 576 << (code_bloc - 2),
        6 => {
            i += 1;
            u64::from(*o.get(i - 1)?) + 1
        }
        7 => {
            i += 2;
            u64::from(u16::from_be_bytes([*o.get(i - 2)?, *o.get(i - 1)?])) + 1
        }
        _ => 256 << (code_bloc - 8),
    };
    let frequence = match code_freq {
        0 => flux.frequence,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => {
            i += 1;
            u32::from(*o.get(i - 1)?) * 1000
        }
        13 => {
            i += 2;
            u32::from(u16::from_be_bytes([*o.get(i - 2)?, *o.get(i - 1)?]))
        }
        _ => {
            i += 2;
            u32::from(u16::from_be_bytes([*o.get(i - 2)?, *o.get(i - 1)?])) * 10
        }
    };
    if flux.frequence != 0 && frequence != flux.frequence {
        return None;
    }
    if crc8(&o[..i]) != *o.get(i)? {
        return None;
    }
    Some(Entete {
        longueur: i + 1,
        variable,
        numero,
        bloc,
    })
}

/// Le premier échantillon d'une trame : le numéro de trame fois la taille de
/// bloc nominale (celle de la première trame) en blocs fixes.
fn premier_echantillon(e: &Entete, nominal: u64) -> u64 {
    if e.variable {
        e.numero
    } else {
        e.numero.saturating_mul(nominal)
    }
}

fn flac<R: Read>(
    l: &mut Lecteur<R>,
    mut pos: u64,
    continuer: &mut impl FnMut() -> bool,
) -> io::Result<Analyse> {
    let mut constat = Constat::new(Conteneur::Flac);
    let mut flux = None;
    // Blocs de métadonnées : sautés, sauf STREAMINFO.
    loop {
        if !continuer() {
            return Ok(Analyse::Interrompu);
        }
        l.oublier(pos);
        if !l.disponible(pos, 4)? {
            constat.tronque = Some(Lieu {
                numero: 0,
                octet: pos,
                echantillon: None,
            });
            return Ok(Analyse::Constat(constat));
        }
        let h = l.tranche(pos, 4);
        let (dernier, genre) = (h[0] & 0x80 != 0, h[0] & 0x7f);
        let longueur = u64::from(u32::from_be_bytes([0, h[1], h[2], h[3]]));
        if genre == 0 && longueur >= 34 {
            if !l.disponible(pos + 4, 34)? {
                constat.tronque = Some(Lieu {
                    numero: 0,
                    octet: pos,
                    echantillon: None,
                });
                return Ok(Analyse::Constat(constat));
            }
            let s = l.tranche(pos + 4, 34);
            flux = Some(Flux {
                trame_min: u32::from_be_bytes([0, s[4], s[5], s[6]]),
                frequence: (u32::from(s[10]) << 12)
                    | (u32::from(s[11]) << 4)
                    | u32::from(s[12] >> 4),
                canaux: ((s[12] >> 1) & 7) + 1,
                bits: (((s[12] & 1) << 4) | (s[13] >> 4)) + 1,
                echantillons: (u64::from(s[13] & 0x0f) << 32)
                    | u64::from(u32::from_be_bytes([s[14], s[15], s[16], s[17]])),
            });
        }
        pos += 4 + longueur;
        if dernier {
            break;
        }
    }
    let Some(flux) = flux else {
        constat.faute(
            Lieu {
                numero: 0,
                octet: 4,
                echantillon: None,
            },
            "entete_illisible",
            "Bloc STREAMINFO absent ou invalide".into(),
        );
        return Ok(Analyse::Constat(constat));
    };

    // Première trame : normalement juste après les métadonnées.
    let mut p = pos;
    let mut courant = loop {
        if !l.disponible(p, 1)? {
            // Aucune trame : coupé juste après les métadonnées.
            constat.tronque = Some(Lieu {
                numero: 0,
                octet: pos,
                echantillon: Some(0),
            });
            return Ok(Analyse::Constat(constat));
        }
        if let Some(e) = entete(l.au_plus(p, 22)?, &flux) {
            break e;
        }
        p += 1;
    };
    if p != pos {
        constat.faute(
            Lieu {
                numero: 0,
                octet: pos,
                echantillon: Some(0),
            },
            "entete_illisible",
            format!("{} octets illisibles avant la première trame", p - pos),
        );
    }
    let nominal = courant.bloc;
    let mut numero = 0u64;
    loop {
        if !continuer() {
            return Ok(Analyse::Interrompu);
        }
        l.oublier(p);
        constat.noter_debut(p);
        let debut = premier_echantillon(&courant, nominal);
        let mut crc = l
            .tranche(p, courant.longueur)
            .iter()
            .fold(0, |c, &o| crc16(c, o));
        let mut q = p + courant.longueur as u64;
        // Une position où la trame en cours pourrait finir intacte (CRC-16
        // nul devant une synchro) : l'en-tête qui suit est alors l'abîmé.
        let mut fin_intacte = None;
        let suivante = loop {
            if !l.disponible(q, 1)? {
                break None;
            }
            let o = l.octet(q);
            // Une étiquette ID3v1 finale (128 octets) n'est pas une trame.
            if o == b'T'
                && l.disponible(q, 128)?
                && !l.disponible(q, 129)?
                && l.tranche(q, 3) == b"TAG"
            {
                break None;
            }
            if o == 0xFF {
                if let Some(e) = entete(l.au_plus(q, 22)?, &flux) {
                    let suite = premier_echantillon(&e, nominal);
                    // La numérotation doit avancer, et de peu : un faux
                    // en-tête au CRC-8 juste n'a presque jamais ce numéro.
                    if e.variable == courant.variable
                        && suite > debut
                        && suite <= debut.saturating_add(64 * courant.bloc.max(nominal))
                    {
                        break Some((q, e));
                    }
                }
                if crc == 0 && fin_intacte.is_none() && q >= p + courant.longueur as u64 + 2 {
                    let synchro = l.au_plus(q, 2)?;
                    if synchro.len() == 2 && synchro[1] & 0xFE == 0xF8 {
                        fin_intacte = Some(q);
                    }
                }
            }
            crc = crc16(crc, o);
            q += 1;
        };
        let attendu = debut + courant.bloc;
        match suivante {
            Some((q, e)) => {
                let suite = premier_echantillon(&e, nominal);
                if crc == 0 && suite != attendu {
                    constat.faute(
                        Lieu {
                            numero: numero + 1,
                            octet: q,
                            echantillon: Some(attendu),
                        },
                        "trames_manquantes",
                        format!("Échantillons {attendu} à {suite} absents"),
                    );
                } else if crc != 0 {
                    match fin_intacte {
                        Some(fin) => {
                            // La trame en cours finit intacte en `fin` ; celle
                            // qui suit a un en-tête abîmé.
                            numero += 1;
                            constat.noter_debut(fin);
                            constat.faute(
                                Lieu {
                                    numero,
                                    octet: fin,
                                    echantillon: Some(attendu),
                                },
                                "crc_trame",
                                "En-tête de trame invalide (CRC-8)".into(),
                            );
                        }
                        None => constat.faute(
                            Lieu {
                                numero,
                                octet: p,
                                echantillon: Some(debut),
                            },
                            "crc_trame",
                            "CRC-16 de la trame incorrect".into(),
                        ),
                    }
                }
                numero += 1;
                p = q;
                courant = e;
            }
            None => {
                let complete = crc == 0 && q >= p + courant.longueur as u64 + 2;
                let manque = flux.echantillons > 0 && attendu < flux.echantillons;
                if complete {
                    if manque {
                        constat.tronque = Some(Lieu {
                            numero: numero + 1,
                            octet: q,
                            echantillon: Some(attendu),
                        });
                    }
                } else if manque || q - p < u64::from(flux.trame_min) {
                    // Des trames devaient suivre, ou la dernière est plus
                    // courte que la plus petite trame annoncée : coupé.
                    constat.tronque = Some(Lieu {
                        numero,
                        octet: p,
                        echantillon: Some(debut),
                    });
                } else {
                    constat.faute(
                        Lieu {
                            numero,
                            octet: p,
                            echantillon: Some(debut),
                        },
                        "crc_trame",
                        "CRC-16 de la dernière trame incorrect".into(),
                    );
                }
                constat.unites = numero + 1;
                return Ok(Analyse::Constat(constat));
            }
        }
    }
}

/// État d'un flux logique Ogg.
struct Serie {
    sequence: u32,
    granule: Option<u64>,
    fin: bool,
}

fn ogg<R: Read>(l: &mut Lecteur<R>, continuer: &mut impl FnMut() -> bool) -> io::Result<Analyse> {
    let mut constat = Constat::new(Conteneur::Ogg);
    let mut series: std::collections::HashMap<u32, Serie> = Default::default();
    let mut p = 0u64;
    let mut numero = 0u64;
    // Après une page fautive, sa longueur et son numéro ne sont pas sûrs :
    // ni la resynchronisation ni le numéro suivant ne comptent une faute de
    // plus.
    let mut apres_faute = false;
    loop {
        if !continuer() {
            return Ok(Analyse::Interrompu);
        }
        l.oublier(p);
        if !l.disponible(p, 1)? {
            break;
        }
        if !l.disponible(p, 4)? || l.tranche(p, 4) != b"OggS" {
            let mut q = p + 1;
            while l.disponible(q, 4)? && l.tranche(q, 4) != b"OggS" {
                q += 1;
            }
            if !apres_faute {
                constat.faute(
                    Lieu {
                        numero,
                        octet: p,
                        echantillon: None,
                    },
                    "donnees_hors_page",
                    format!("Octets hors de toute page à la position {p}"),
                );
            }
            apres_faute = true;
            if !l.disponible(q, 4)? {
                break;
            }
            p = q;
            continue;
        }
        let tronque = |constat: &mut Constat, echantillon| {
            constat.tronque = Some(Lieu {
                numero,
                octet: p,
                echantillon,
            });
        };
        if !l.disponible(p, 27)? {
            tronque(&mut constat, None);
            break;
        }
        let segments = usize::from(l.octet(p + 26));
        if !l.disponible(p, 27 + segments)? {
            tronque(&mut constat, None);
            break;
        }
        let corps: usize = l
            .tranche(p + 27, segments)
            .iter()
            .map(|&o| usize::from(o))
            .sum();
        let taille = 27 + segments + corps;
        let h = l.tranche(p, 27);
        let drapeaux = h[5];
        let granule = i64::from_le_bytes(h[6..14].try_into().unwrap());
        let serie = u32::from_le_bytes(h[14..18].try_into().unwrap());
        let sequence = u32::from_le_bytes(h[18..22].try_into().unwrap());
        let lu = u32::from_le_bytes(h[22..26].try_into().unwrap());
        let echantillon = series.get(&serie).map_or(Some(0), |s| s.granule);
        if !l.disponible(p, taille)? {
            tronque(&mut constat, echantillon);
            break;
        }
        constat.noter_debut(p);
        let calcule = l
            .tranche(p, taille)
            .iter()
            .enumerate()
            .fold(0, |c, (i, &o)| {
                crc32(c, if (22..26).contains(&i) { 0 } else { o })
            });
        let lieu = Lieu {
            numero,
            octet: p,
            echantillon,
        };
        if calcule != lu {
            constat.faute(lieu, "crc_page", "CRC-32 de la page incorrect".into());
            apres_faute = true;
        } else {
            match series.get(&serie) {
                Some(s) if !apres_faute && sequence != s.sequence.wrapping_add(1) => constat.faute(
                    lieu,
                    "pages_manquantes",
                    format!(
                        "Page {} attendue, page {sequence} trouvée",
                        s.sequence.wrapping_add(1)
                    ),
                ),
                _ => {}
            }
            let s = series.entry(serie).or_insert(Serie {
                sequence,
                granule: Some(0),
                fin: false,
            });
            s.sequence = sequence;
            if granule >= 0 {
                s.granule = Some(granule as u64);
            }
            s.fin |= drapeaux & 0x04 != 0;
            apres_faute = false;
        }
        numero += 1;
        p += taille as u64;
    }
    // Un flux sans page de fin : le fichier s'arrête avant elle.
    if constat.tronque.is_none()
        && !apres_faute
        && let Some(s) = series.values().find(|s| !s.fin)
    {
        constat.tronque = Some(Lieu {
            numero,
            octet: p,
            echantillon: s.granule,
        });
    }
    if series.is_empty() && constat.premiere_faute.is_none() && constat.tronque.is_none() {
        constat.faute(
            Lieu {
                numero: 0,
                octet: 0,
                echantillon: None,
            },
            "entete_illisible",
            "Aucune page Ogg valide".into(),
        );
    }
    constat.unites = numero;
    Ok(Analyse::Constat(constat))
}

#[cfg(test)]
#[path = "crc_tests_7847.rs"]
mod tests;
