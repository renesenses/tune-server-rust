//! Lire un CD audio par le volume que macOS en monte (`cddafs`).
//!
//! Sous macOS, le noyau ne laisse pas un processus ordinaire parler au lecteur
//! par ioctl : un CD audio inséré est monté sous `/Volumes/<titre>/` par le
//! système de fichiers `cddafs`, qui présente chaque piste audio comme un
//! fichier AIFF VIRTUEL (`1 Audio Track.aiff`…) — lire le fichier lit les
//! secteurs du disque — et la table des pistes dans un fichier caché,
//! `.TOC.plist`, à la racine du volume.
//!
//! Ce module ne contient AUCUN appel propre à macOS : il ne lit que des
//! fichiers ordinaires. Il se compile et se teste partout (Linux compris) sur
//! de faux volumes — un dossier, des AIFF et un `.TOC.plist` fabriqués. Seule
//! la DÉCOUVERTE du volume (`getfsstat`, type `cddafs`) et celle du lecteur
//! (IOKit) vivent dans `macos.rs`.
//!
//! * [`plist`] : un lecteur de plist XML réduit à ce que `.TOC.plist` porte ;
//! * [`toc_du_plist`] : la TOC, depuis la TOC BRUTE du lecteur (`Format 0x02
//!   TOC Data`, MSF absolus) ou, à défaut, depuis `Sessions` (blocs LBA) ;
//! * [`entete_aiff`] : l'en-tête AIFF/AIFC, jusqu'au chunk `SSND` et son
//!   décalage, sans supposer de taille fixe ;
//! * [`plan_de_lecture`] : la correspondance secteur ↔ (fichier, octet) ;
//! * [`LecteurVolume`] : le `LecteurDisque` sur un volume, avec l'échange
//!   d'octets gros-boutiste → petit-boutiste (l'ordre des secteurs bruts que
//!   rend `CDROMREADAUDIO` sous Linux, et celui du WAV servi à la zone).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::lecteur::{ErreurCd, LecteurDisque, Presence};
use crate::toc::{OCTETS_PAR_SECTEUR, PREGAP, PisteToc, TRAMES_PAR_SECTEUR, Toc};

/// Le nom du fichier de TOC à la racine du volume.
pub const FICHIER_TOC: &str = ".TOC.plist";

// ─────────────────────────────────────────────────────────────────────────
// Plist XML
// ─────────────────────────────────────────────────────────────────────────

pub mod plist {
    //! Un lecteur de plist XML volontairement petit : `dict`, `array`, `key`,
    //! `string`, `integer`, `real`, `data`, `date`, `true`, `false`. Assez pour
    //! `.TOC.plist`, sans dépendance nouvelle. Une plist BINAIRE (`bplist00`)
    //! est refusée par une erreur claire.

    use base64::Engine;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Valeur {
        Dict(Vec<(String, Valeur)>),
        Tableau(Vec<Valeur>),
        Texte(String),
        Entier(i64),
        Reel(f64),
        Donnees(Vec<u8>),
        Booleen(bool),
    }

    impl Valeur {
        /// La valeur d'une clé d'un dictionnaire.
        pub fn cle(&self, nom: &str) -> Option<&Valeur> {
            match self {
                Valeur::Dict(v) => v.iter().find(|(k, _)| k == nom).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn entier(&self) -> Option<i64> {
            match self {
                Valeur::Entier(i) => Some(*i),
                Valeur::Reel(r) if r.fract() == 0.0 => Some(*r as i64),
                Valeur::Texte(t) => t.trim().parse().ok(),
                _ => None,
            }
        }
        pub fn booleen(&self) -> Option<bool> {
            match self {
                Valeur::Booleen(b) => Some(*b),
                Valeur::Entier(i) => Some(*i != 0),
                _ => None,
            }
        }
        pub fn tableau(&self) -> Option<&[Valeur]> {
            match self {
                Valeur::Tableau(v) => Some(v),
                _ => None,
            }
        }
    }

    struct Curseur<'a> {
        s: &'a str,
        i: usize,
    }

    enum Balise<'a> {
        Ouvrante(&'a str),
        Fermante,
        Vide(&'a str),
    }

    impl<'a> Curseur<'a> {
        fn reste(&self) -> &'a str {
            &self.s[self.i..]
        }

        /// Saute texte blanc, déclarations (`<?…?>`, `<!…>`) et commentaires.
        fn sauter(&mut self) -> Result<(), String> {
            loop {
                let r = self.reste();
                let blanc = r.len() - r.trim_start().len();
                self.i += blanc;
                let r = self.reste();
                let fin = if r.starts_with("<!--") {
                    r.find("-->").map(|p| p + 3)
                } else if r.starts_with("<?") {
                    r.find("?>").map(|p| p + 2)
                } else if r.starts_with("<!") {
                    r.find('>').map(|p| p + 1)
                } else {
                    return Ok(());
                };
                self.i += fin.ok_or("plist : déclaration non fermée")?;
            }
        }

        fn balise(&mut self) -> Result<Balise<'a>, String> {
            self.sauter()?;
            let r = self.reste();
            if !r.starts_with('<') {
                return Err(format!(
                    "plist : balise attendue à l'octet {} ({:?})",
                    self.i,
                    r.chars().take(20).collect::<String>()
                ));
            }
            let fin = r.find('>').ok_or("plist : balise non fermée")?;
            let dedans = &r[1..fin];
            self.i += fin + 1;
            let nom = |t: &'a str| t.split_whitespace().next().unwrap_or("");
            Ok(if dedans.starts_with('/') {
                Balise::Fermante
            } else if let Some(n) = dedans.strip_suffix('/') {
                Balise::Vide(nom(n))
            } else {
                Balise::Ouvrante(nom(dedans))
            })
        }

        /// Le texte jusqu'à `</nom>`, balise fermante consommée.
        fn texte_jusqu_a(&mut self, nom: &str) -> Result<&'a str, String> {
            let fermante = format!("</{nom}>");
            let r = self.reste();
            let p = r
                .find(&fermante)
                .ok_or_else(|| format!("plist : <{nom}> non fermée"))?;
            self.i += p + fermante.len();
            Ok(&r[..p])
        }

        fn valeur(&mut self) -> Result<Option<Valeur>, String> {
            let b = self.balise()?;
            let nom = match b {
                Balise::Fermante => return Ok(None),
                Balise::Vide(n) => {
                    return Ok(Some(match n {
                        "true" => Valeur::Booleen(true),
                        "false" => Valeur::Booleen(false),
                        "dict" => Valeur::Dict(Vec::new()),
                        "array" => Valeur::Tableau(Vec::new()),
                        "string" => Valeur::Texte(String::new()),
                        "data" => Valeur::Donnees(Vec::new()),
                        autre => return Err(format!("plist : <{autre}/> inattendue")),
                    }));
                }
                Balise::Ouvrante(n) => n,
            };
            Ok(Some(match nom {
                "plist" => {
                    let v = self.valeur()?.ok_or("plist : <plist> vide")?;
                    // La fermante `</plist>` (on tolère qu'elle manque).
                    let _ = self.balise();
                    v
                }
                "dict" => {
                    let mut entrees = Vec::new();
                    loop {
                        match self.balise()? {
                            Balise::Fermante => break,
                            Balise::Ouvrante("key") => {
                                let k = decoder(self.texte_jusqu_a("key")?);
                                let v = self
                                    .valeur()?
                                    .ok_or_else(|| format!("plist : clé « {k} » sans valeur"))?;
                                entrees.push((k, v));
                            }
                            Balise::Vide("key") => {
                                let v = self.valeur()?.ok_or("plist : clé vide sans valeur")?;
                                entrees.push((String::new(), v));
                            }
                            _ => return Err("plist : <key> attendue dans un <dict>".into()),
                        }
                    }
                    Valeur::Dict(entrees)
                }
                "array" => {
                    let mut v = Vec::new();
                    while let Some(e) = self.valeur()? {
                        v.push(e);
                    }
                    Valeur::Tableau(v)
                }
                "string" | "date" => Valeur::Texte(decoder(self.texte_jusqu_a(nom)?)),
                "integer" => {
                    let t = self.texte_jusqu_a("integer")?.trim();
                    let i = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                        i64::from_str_radix(h, 16).ok()
                    } else {
                        t.parse().ok()
                    };
                    Valeur::Entier(i.ok_or_else(|| format!("plist : entier illisible {t:?}"))?)
                }
                "real" => {
                    let t = self.texte_jusqu_a("real")?.trim();
                    Valeur::Reel(
                        t.parse()
                            .map_err(|_| format!("plist : réel illisible {t:?}"))?,
                    )
                }
                "data" => {
                    let t: String = self
                        .texte_jusqu_a("data")?
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    Valeur::Donnees(
                        base64::engine::general_purpose::STANDARD
                            .decode(t.as_bytes())
                            .map_err(|e| format!("plist : <data> illisible : {e}"))?,
                    )
                }
                "true" => {
                    self.texte_jusqu_a("true")?;
                    Valeur::Booleen(true)
                }
                "false" => {
                    self.texte_jusqu_a("false")?;
                    Valeur::Booleen(false)
                }
                autre => return Err(format!("plist : <{autre}> inattendue")),
            }))
        }
    }

    fn decoder(t: &str) -> String {
        t.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    pub fn lire(octets: &[u8]) -> Result<Valeur, String> {
        if octets.starts_with(b"bplist") {
            return Err("plist binaire : format non pris en charge".into());
        }
        let s = std::str::from_utf8(octets).map_err(|_| "plist : pas de l'UTF-8")?;
        let s = s.strip_prefix('\u{feff}').unwrap_or(s);
        Curseur { s, i: 0 }
            .valeur()?
            .ok_or_else(|| "plist : vide".to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// TOC
// ─────────────────────────────────────────────────────────────────────────

/// Clé de la TOC brute (réponse READ TOC format 2 : « full TOC », MSF).
const CLE_TOC_BRUTE: &str = "Format 0x02 TOC Data";
const POINT_FIN: u8 = 0xA2;

/// Un MSF absolu (avec les 2 s de pré-gap) → LBA.
fn lba_de_msf(m: u8, s: u8, f: u8) -> Option<u32> {
    ((m as u32 * 60 + s as u32) * 75 + f as u32).checked_sub(PREGAP)
}

/// La TOC depuis la TOC BRUTE du lecteur (`CDTOC` d'IOKit) : 2 octets de
/// longueur (gros-boutiste, sans eux-mêmes), première et dernière session,
/// puis des descripteurs de 11 octets : session, ADR (4 bits hauts) et
/// CONTROL (4 bits bas), TNO, POINT, MIN SEC FRAME, zéro, PMIN PSEC PFRAME.
/// POINT 1..=99 = début de piste (PMSF), `0xA2` = lead-out de la session.
/// Seuls les descripteurs ADR 1 (position) comptent.
pub fn toc_de_la_toc_brute(octets: &[u8]) -> Result<Toc, String> {
    if octets.len() < 4 {
        return Err("TOC brute trop courte".into());
    }
    let longueur = u16::from_be_bytes([octets[0], octets[1]]) as usize;
    let fin_donnees = (longueur + 2).min(octets.len());
    let mut pistes: Vec<PisteToc> = Vec::new();
    // (session, lba) du lead-out : on garde celui de la DERNIÈRE session, comme
    // `CDROM_LEADOUT` sous Linux.
    let mut fin: Option<(u8, u32)> = None;
    for d in octets[4..fin_donnees].chunks_exact(11) {
        let (session, adr, control, point) = (d[0], d[1] >> 4, d[1] & 0x0F, d[3]);
        if adr != 1 {
            continue;
        }
        let Some(lba) = lba_de_msf(d[8], d[9], d[10]) else {
            continue;
        };
        match point {
            1..=99 if !pistes.iter().any(|p| p.numero == point) => {
                pistes.push(PisteToc {
                    numero: point,
                    debut: lba,
                    audio: control & 0x04 == 0,
                });
            }
            POINT_FIN if fin.is_none_or(|(s, _)| session >= s) => fin = Some((session, lba)),
            _ => {}
        }
    }
    pistes.sort_by_key(|p| p.numero);
    let (_, fin) = fin.ok_or("TOC brute sans lead-out")?;
    Toc::nouvelle(pistes, fin)
}

/// La TOC depuis `Sessions` : chaque session porte `Track Array` (un dict par
/// piste : `Point`, `Start Block`, `Data`) et `Leadout Block`.
///
/// CONSTATÉ sur un vrai disque (SuperDrive, 25/09/2026) : `Start Block` et
/// `Leadout Block` sont des adresses ABSOLUES, en trames MSF — pré-gap de 150
/// COMPRIS. La piste 1 du disque d'essai y vaut 225, et sa TOC brute porte
/// 00:03:00 (= 225 trames) : LBA 75. On retire donc le pré-gap.
pub fn toc_des_sessions(racine: &plist::Valeur) -> Result<Toc, String> {
    let sessions = racine
        .cle("Sessions")
        .and_then(|s| s.tableau())
        .ok_or("plist sans « Sessions »")?;
    let mut pistes: Vec<PisteToc> = Vec::new();
    let mut fin: Option<(i64, u32)> = None;
    for (i, s) in sessions.iter().enumerate() {
        let numero_session = s
            .cle("Session Number")
            .and_then(|v| v.entier())
            .unwrap_or(i as i64 + 1);
        if let Some(l) = s.cle("Leadout Block").and_then(|v| v.entier())
            && fin.is_none_or(|(n, _)| numero_session >= n)
        {
            let l = u32::try_from(l)
                .ok()
                .and_then(|l| l.checked_sub(PREGAP))
                .ok_or("lead-out avant le pré-gap")?;
            fin = Some((numero_session, l));
        }
        for p in s
            .cle("Track Array")
            .and_then(|t| t.tableau())
            .unwrap_or(&[])
        {
            let (Some(point), Some(debut)) = (
                p.cle("Point").and_then(|v| v.entier()),
                p.cle("Start Block").and_then(|v| v.entier()),
            ) else {
                continue;
            };
            let (Ok(numero @ 1..=99), Some(debut)) = (
                u8::try_from(point),
                u32::try_from(debut)
                    .ok()
                    .and_then(|d| d.checked_sub(PREGAP)),
            ) else {
                continue;
            };
            let audio = !p.cle("Data").and_then(|v| v.booleen()).unwrap_or(false);
            pistes.push(PisteToc {
                numero,
                debut,
                audio,
            });
        }
    }
    pistes.sort_by_key(|p| p.numero);
    pistes.dedup_by_key(|p| p.numero);
    let (_, fin) = fin.ok_or("plist sans « Leadout Block »")?;
    Toc::nouvelle(pistes, fin)
}

/// La TOC de `.TOC.plist` : la TOC brute d'abord (c'est ce que le lecteur a
/// rendu), `Sessions` sinon. Quand les deux sont là et divergent, la brute
/// l'emporte et la divergence est journalisée.
pub fn toc_du_plist(octets: &[u8]) -> Result<Toc, String> {
    let racine = plist::lire(octets)?;
    let brute = match racine.cle(CLE_TOC_BRUTE) {
        Some(plist::Valeur::Donnees(d)) => Some(toc_de_la_toc_brute(d)),
        _ => None,
    };
    let sessions = racine.cle("Sessions").map(|_| toc_des_sessions(&racine));
    match (brute, sessions) {
        (Some(Ok(b)), Some(Ok(s))) => {
            if b != s {
                tracing::warn!(brute = ?b, sessions = ?s, "cd_toc_plist_divergente");
            }
            Ok(b)
        }
        (Some(Ok(b)), _) => Ok(b),
        (_, Some(Ok(s))) => Ok(s),
        (Some(Err(e)), _) | (None, Some(Err(e))) => Err(e),
        (None, None) => Err("plist sans TOC (ni TOC brute, ni « Sessions »)".into()),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// AIFF
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnteteAiff {
    pub canaux: u16,
    pub bits: u16,
    pub frequence: u32,
    /// Trames annoncées par `COMM`.
    pub trames: u64,
    /// Premier octet du PCM dans le fichier : début des données du chunk
    /// `SSND` + 8 (offset, blockSize) + `offset`.
    pub debut_donnees: u64,
    /// `true` pour un AIFC `sowt` (petit-boutiste) ; `false` pour l'AIFF et
    /// l'AIFC `NONE` (gros-boutistes).
    pub petit_boutiste: bool,
}

/// Le flottant étendu 80 bits de `COMM` (fréquence), en entier.
fn frequence_ieee_etendu(o: &[u8]) -> u32 {
    let exp = (u16::from_be_bytes([o[0], o[1]]) & 0x7FFF) as i32;
    let mantisse = u64::from_be_bytes(o[2..10].try_into().unwrap_or([0; 8]));
    if exp == 0 || mantisse == 0 {
        return 0;
    }
    let decalage = exp - 16_383 - 63;
    let v = if decalage >= 0 {
        mantisse.checked_shl(decalage as u32).unwrap_or(0) as f64
    } else {
        mantisse as f64 / 2f64.powi(-decalage)
    };
    v.round() as u32
}

/// L'en-tête d'un AIFF/AIFC contenu dans `octets` (le début du fichier).
pub fn entete_aiff(octets: &[u8]) -> Result<EnteteAiff, String> {
    entete_aiff_par(|pos, n| {
        let d = pos as usize;
        octets
            .get(d..d.saturating_add(n))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "en-tête AIFF tronqué".to_string())
    })
}

/// L'en-tête d'un AIFF/AIFC jusqu'au chunk `SSND`, lu par `lire(position,
/// longueur)`. Aucune taille d'en-tête n'est supposée : les chunks sont
/// parcourus un à un (taille paire, octet de bourrage), dans n'importe quel
/// ordre tant que `COMM` précède `SSND`. Seuls les en-têtes de chunk, `COMM`
/// et les 8 premiers octets de `SSND` sont lus : sur un volume `cddafs`,
/// ouvrir un disque ne fait tourner aucun secteur audio.
pub fn entete_aiff_par(
    mut lire: impl FnMut(u64, usize) -> Result<Vec<u8>, String>,
) -> Result<EnteteAiff, String> {
    let tete = lire(0, 12).map_err(|_| "pas un fichier AIFF (trop court)".to_string())?;
    if &tete[0..4] != b"FORM" {
        return Err("pas un fichier AIFF (FORM absent)".into());
    }
    let aifc = match &tete[8..12] {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return Err("pas un fichier AIFF (ni AIFF, ni AIFC)".into()),
    };
    let fin_form = 8 + u32::from_be_bytes([tete[4], tete[5], tete[6], tete[7]]) as u64;
    let mut i = 12u64;
    let mut comm: Option<(u16, u64, u16, u32, bool)> = None;
    // Borne de sécurité : un en-tête ne compte pas des centaines de chunks.
    for _ in 0..64 {
        if i + 8 > fin_form {
            break;
        }
        let Ok(t) = lire(i, 8) else { break };
        let taille = u32::from_be_bytes([t[4], t[5], t[6], t[7]]) as u64;
        let donnees = i + 8;
        match &t[0..4] {
            b"COMM" => {
                let n = if aifc { 22 } else { 18 };
                let c = lire(donnees, n.min(taille as usize).max(18))
                    .map_err(|_| "chunk COMM tronqué".to_string())?;
                let canaux = u16::from_be_bytes([c[0], c[1]]);
                let trames = u32::from_be_bytes([c[2], c[3], c[4], c[5]]) as u64;
                let bits = u16::from_be_bytes([c[6], c[7]]);
                let frequence = frequence_ieee_etendu(&c[8..18]);
                let petit = match c.get(18..22) {
                    Some(b"sowt") if aifc => true,
                    Some(b"NONE") | Some(b"twos") | None => false,
                    Some(_) if !aifc => false,
                    Some(autre) => {
                        return Err(format!(
                            "AIFC compressé ({}) : non pris en charge",
                            String::from_utf8_lossy(autre)
                        ));
                    }
                };
                comm = Some((canaux, trames, bits, frequence, petit));
            }
            b"SSND" => {
                let (canaux, trames, bits, frequence, petit_boutiste) =
                    comm.ok_or("chunk SSND avant COMM")?;
                let s = lire(donnees, 8).map_err(|_| "chunk SSND tronqué".to_string())?;
                let offset = u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as u64;
                return Ok(EnteteAiff {
                    canaux,
                    bits,
                    frequence,
                    trames,
                    debut_donnees: donnees + 8 + offset,
                    petit_boutiste,
                });
            }
            _ => {}
        }
        i = donnees + taille + (taille & 1);
    }
    Err("chunk SSND introuvable dans l'en-tête".into())
}

/// L'en-tête d'un fichier AIFF sur disque, lu chunk par chunk.
pub fn entete_du_fichier(chemin: &Path) -> Result<EnteteAiff, String> {
    let mut f = File::open(chemin).map_err(|e| format!("illisible : {e}"))?;
    entete_aiff_par(|pos, n| {
        let mut v = vec![0; n];
        f.seek(SeekFrom::Start(pos))
            .and_then(|_| f.read_exact(&mut v))
            .map_err(|e| e.to_string())?;
        Ok(v)
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Le volume
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FichierPiste {
    pub numero: u8,
    pub chemin: PathBuf,
    /// Premier secteur (LBA) que le fichier porte : le début de sa piste.
    pub debut: u32,
    /// Secteurs entiers que le fichier porte.
    pub secteurs: u32,
    pub debut_donnees: u64,
    pub petit_boutiste: bool,
}

/// Un morceau d'une lecture : `octets` octets du fichier `fichier` (indice),
/// à partir de l'octet `position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Morceau {
    pub fichier: usize,
    pub position: u64,
    pub octets: usize,
}

/// La correspondance secteurs → fichiers : `[lba, lba + nombre)` découpé en
/// morceaux de fichiers consécutifs. Une lecture qui enjambe deux pistes
/// rend deux morceaux. Un secteur qu'aucun fichier ne porte est une erreur
/// (rendue avec son LBA).
pub fn plan_de_lecture(
    fichiers: &[FichierPiste],
    lba: u32,
    nombre: u32,
) -> Result<Vec<Morceau>, u32> {
    let mut plan = Vec::new();
    let (mut s, fin) = (lba, lba.saturating_add(nombre));
    while s < fin {
        let (i, f) = fichiers
            .iter()
            .enumerate()
            .find(|(_, f)| s >= f.debut && s < f.debut + f.secteurs)
            .ok_or(s)?;
        let n = (fin - s).min(f.debut + f.secteurs - s);
        plan.push(Morceau {
            fichier: i,
            position: f.debut_donnees + (s - f.debut) as u64 * OCTETS_PAR_SECTEUR as u64,
            octets: n as usize * OCTETS_PAR_SECTEUR,
        });
        s += n;
    }
    Ok(plan)
}

/// Échange les octets de chaque échantillon 16 bits : gros ↔ petit-boutiste.
pub fn echanger_octets(tampon: &mut [u8]) {
    for e in tampon.chunks_exact_mut(2) {
        e.swap(0, 1);
    }
}

/// Le numéro de piste d'un fichier du volume : `12 Audio Track.aiff` → 12.
/// Le nom est localisable ; seul compte le nombre qui le commence (ou, à
/// défaut, le premier nombre du nom).
pub fn numero_du_fichier(nom: &str) -> Option<u8> {
    let bas = nom.to_ascii_lowercase();
    if nom.starts_with('.') || !(bas.ends_with(".aiff") || bas.ends_with(".aif")) {
        return None;
    }
    let chiffres: String = nom
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    chiffres.parse().ok().filter(|n| (1..=99).contains(n))
}

/// Ce que l'on sait d'un volume ouvert : la TOC et les fichiers des pistes.
#[derive(Debug)]
pub struct VolumeCdda {
    pub racine: PathBuf,
    pub toc: Toc,
    pub fichiers: Vec<FichierPiste>,
}

impl VolumeCdda {
    /// Ouvre un volume : `.TOC.plist`, puis l'en-tête de chaque AIFF.
    ///
    /// Sans `.TOC.plist` lisible, la TOC est déduite des fichiers (pistes
    /// bout à bout depuis le secteur 0) : la lecture marche, mais
    /// l'identifiant de disque peut être faux — c'est journalisé.
    pub fn ouvrir(racine: &Path) -> Result<VolumeCdda, String> {
        let mut entrees: Vec<(u8, PathBuf)> = std::fs::read_dir(racine)
            .map_err(|e| format!("{} illisible : {e}", racine.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let nom = e.file_name().to_string_lossy().into_owned();
                numero_du_fichier(&nom).map(|n| (n, e.path()))
            })
            .collect();
        entrees.sort();
        entrees.dedup_by_key(|(n, _)| *n);
        if entrees.is_empty() {
            return Err(format!("{} : aucune piste AIFF", racine.display()));
        }
        let mut entetes = Vec::with_capacity(entrees.len());
        for (numero, chemin) in &entrees {
            let e = entete_du_fichier(chemin).map_err(|e| format!("{} : {e}", chemin.display()))?;
            if (e.canaux, e.bits, e.frequence) != (2, 16, 44_100) {
                return Err(format!(
                    "{} : {} canaux, {} bits, {} Hz — pas du CD audio",
                    chemin.display(),
                    e.canaux,
                    e.bits,
                    e.frequence
                ));
            }
            entetes.push((*numero, chemin.clone(), e));
        }

        let toc = match std::fs::read(racine.join(FICHIER_TOC)) {
            Ok(o) => toc_du_plist(&o),
            Err(e) => Err(format!("{FICHIER_TOC} illisible : {e}")),
        };
        let toc = match toc {
            Ok(t) => t,
            Err(raison) => {
                tracing::warn!(%raison, "cd_toc_deduite_des_fichiers");
                let mut debut = 0u32;
                let mut pistes = Vec::new();
                for (numero, _, e) in &entetes {
                    pistes.push(PisteToc {
                        numero: *numero,
                        debut,
                        audio: true,
                    });
                    debut += (e.trames / TRAMES_PAR_SECTEUR) as u32;
                }
                Toc::nouvelle(pistes, debut)?
            }
        };

        let mut fichiers = Vec::with_capacity(entetes.len());
        for (numero, chemin, e) in entetes {
            let Some(piste) = toc.piste(numero).filter(|p| p.audio) else {
                tracing::warn!(numero, "cd_fichier_sans_piste_audio_dans_la_toc");
                continue;
            };
            let secteurs = (e.trames / TRAMES_PAR_SECTEUR) as u32;
            if Some(secteurs) != toc.secteurs(numero) {
                tracing::warn!(
                    numero,
                    fichier = secteurs,
                    toc = ?toc.secteurs(numero),
                    "cd_longueur_fichier_differente_de_la_toc"
                );
            }
            fichiers.push(FichierPiste {
                numero,
                chemin,
                debut: piste.debut,
                secteurs,
                debut_donnees: e.debut_donnees,
                petit_boutiste: e.petit_boutiste,
            });
        }
        fichiers.sort_by_key(|f| f.debut);
        Ok(VolumeCdda {
            racine: racine.to_path_buf(),
            toc,
            fichiers,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Le lecteur
// ─────────────────────────────────────────────────────────────────────────

type Localisateur = Box<dyn Fn() -> Option<PathBuf> + Send + Sync>;
type Detecteur = Box<dyn Fn() -> bool + Send + Sync>;

/// Le volume ouvert et ses fichiers, gardés ouverts entre deux lectures.
struct Ouvert {
    volume: Arc<VolumeCdda>,
    descripteurs: Vec<Option<File>>,
}

/// Un `LecteurDisque` sur un volume de CD audio.
///
/// `trouver` dit où est le volume (aucun ⇒ pas de disque) ; `lecteur_present`
/// distingue « lecteur vide » de « aucun lecteur ». Sous macOS ce sont
/// `getfsstat` et IOKit ; dans les tests, un dossier fabriqué.
pub struct LecteurVolume {
    nom: String,
    trouver: Localisateur,
    lecteur_present: Detecteur,
    ouvert: Mutex<Option<Ouvert>>,
}

impl LecteurVolume {
    pub fn new(
        nom: impl Into<String>,
        trouver: impl Fn() -> Option<PathBuf> + Send + Sync + 'static,
        lecteur_present: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            nom: nom.into(),
            trouver: Box::new(trouver),
            lecteur_present: Box::new(lecteur_present),
            ouvert: Mutex::new(None),
        }
    }

    /// Un lecteur sur un dossier fixe (tests, `TUNE_CD_DEVICE`).
    pub fn sur_dossier(racine: PathBuf) -> Self {
        let r = racine.clone();
        Self::new(
            racine.display().to_string(),
            move || r.join(FICHIER_TOC).exists().then(|| r.clone()),
            || true,
        )
    }

    fn volume_present(&self) -> Option<PathBuf> {
        (self.trouver)().filter(|r| r.is_dir())
    }

    /// Ouvre (ou rouvre) le volume courant.
    fn rouvrir(&self) -> Result<Arc<VolumeCdda>, ErreurCd> {
        let racine = self.volume_present().ok_or(ErreurCd::AucunDisque)?;
        let v = Arc::new(VolumeCdda::ouvrir(&racine).map_err(|e| {
            if self.volume_present().is_none() {
                ErreurCd::AucunDisque
            } else {
                ErreurCd::Autre(e)
            }
        })?);
        let n = v.fichiers.len();
        *self.ouvert.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ouvert {
            volume: v.clone(),
            descripteurs: (0..n).map(|_| None).collect(),
        });
        Ok(v)
    }

    fn lire_plan(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        let mut garde = self.ouvert.lock().unwrap_or_else(|e| e.into_inner());
        if garde.is_none() {
            drop(garde);
            self.rouvrir()?;
            garde = self.ouvert.lock().unwrap_or_else(|e| e.into_inner());
        }
        let ouvert = garde.as_mut().ok_or(ErreurCd::AucunDisque)?;
        let plan = plan_de_lecture(&ouvert.volume.fichiers, lba, nombre).map_err(|s| {
            ErreurCd::Lecture {
                lba: s,
                raison: "secteur hors des pistes audio du volume".into(),
            }
        })?;
        let mut o = 0usize;
        for m in plan {
            let f = &ouvert.volume.fichiers[m.fichier];
            let erreur = |e: std::io::Error| ErreurCd::Lecture {
                lba,
                raison: format!("{} : {e}", f.chemin.display()),
            };
            if ouvert.descripteurs[m.fichier].is_none() {
                ouvert.descripteurs[m.fichier] = Some(File::open(&f.chemin).map_err(erreur)?);
            }
            let d = ouvert.descripteurs[m.fichier]
                .as_mut()
                .ok_or(ErreurCd::AucunDisque)?;
            let tranche = &mut sortie[o..o + m.octets];
            d.seek(SeekFrom::Start(m.position)).map_err(erreur)?;
            d.read_exact(tranche).map_err(erreur)?;
            if !f.petit_boutiste {
                echanger_octets(tranche);
            }
            o += m.octets;
        }
        Ok(())
    }
}

impl LecteurDisque for LecteurVolume {
    fn chemin(&self) -> String {
        self.volume_present()
            .map(|r| r.display().to_string())
            .unwrap_or_else(|| self.nom.clone())
    }

    fn presence(&self) -> Presence {
        if self.volume_present().is_some() {
            Presence::Disque
        } else if (self.lecteur_present)() {
            Presence::Vide
        } else {
            Presence::AucunLecteur
        }
    }

    /// Relue à chaque appel : un autre disque peut porter le même nom de
    /// volume (« Audio CD »). Rouvre aussi les fichiers.
    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        Ok(self.rouvrir()?.toc.clone())
    }

    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        if sortie.len() != nombre as usize * OCTETS_PAR_SECTEUR {
            return Err(ErreurCd::Autre("tampon de taille fausse".into()));
        }
        // Un descripteur ouvert survit parfois au démontage (fichier délié) :
        // la présence du volume est vérifiée d'abord, à chaque bloc.
        if self.volume_present().is_none() {
            *self.ouvert.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Err(ErreurCd::AucunDisque);
        }
        let r = self.lire_plan(lba, nombre, sortie);
        if r.is_err() {
            // Comme sous Linux : les descripteurs sont jetés, et un échec sur
            // un volume disparu est une éjection, pas une rayure.
            *self.ouvert.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if self.volume_present().is_none() {
                return Err(ErreurCd::AucunDisque);
            }
        }
        r
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Témoins
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::discid::disc_id;
    use crate::simule::contenu_des_secteurs;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Un dossier temporaire propre au témoin, effacé à la fin.
    pub(crate) struct Dossier(pub PathBuf);
    impl Dossier {
        pub(crate) fn nouveau(nom: &str) -> Dossier {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "tune-cd-{nom}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dossier(p)
        }
    }
    impl Drop for Dossier {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 44 100 Hz en flottant étendu 80 bits.
    const F44100: [u8; 10] = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];

    /// Un AIFF (ou AIFC `sowt`) portant les secteurs `[debut, fin)` du motif
    /// simulé, avec un chunk parasite avant `SSND` et un `offset` non nul :
    /// un lecteur qui supposerait 54 octets d'en-tête lirait faux.
    pub(crate) fn aiff(debut: u32, fin: u32, sowt: bool, offset: u32) -> Vec<u8> {
        let mut pcm = contenu_des_secteurs(debut, fin - debut);
        if !sowt {
            echanger_octets(&mut pcm);
        }
        let trames = (fin - debut) as u64 * TRAMES_PAR_SECTEUR;
        let mut comm = Vec::new();
        comm.extend(2u16.to_be_bytes());
        comm.extend((trames as u32).to_be_bytes());
        comm.extend(16u16.to_be_bytes());
        comm.extend(F44100);
        if sowt {
            comm.extend(b"sowt");
            comm.extend([0u8, 0]); // pstring vide + bourrage
        }
        let mut corps = Vec::new();
        corps.extend(if sowt { b"AIFC" } else { b"AIFF" });
        let mut chunk = |id: &[u8], d: &[u8]| {
            corps.extend(id);
            corps.extend((d.len() as u32).to_be_bytes());
            corps.extend(d);
            if d.len() % 2 == 1 {
                corps.push(0);
            }
        };
        chunk(b"COMM", &comm);
        chunk(b"ANNO", b"Tune #4863");
        let mut ssnd = Vec::new();
        ssnd.extend(offset.to_be_bytes());
        ssnd.extend(0u32.to_be_bytes());
        ssnd.extend(vec![0xEE; offset as usize]);
        ssnd.extend(&pcm);
        chunk(b"SSND", &ssnd);
        let mut f = b"FORM".to_vec();
        f.extend((corps.len() as u32).to_be_bytes());
        f.extend(corps);
        f
    }

    fn msf(lba: u32) -> [u8; 3] {
        let a = lba + PREGAP;
        [(a / 4500) as u8, (a / 75 % 60) as u8, (a % 75) as u8]
    }

    /// La TOC brute (format 2) d'une TOC : A0, A1, A2 et les pistes, par
    /// session (une piste de données ouvre la session 2).
    pub(crate) fn toc_brute(toc: &Toc) -> Vec<u8> {
        let mut d = Vec::new();
        let mut desc = |session: u8, ctrl: u8, point: u8, p: [u8; 3]| {
            d.extend([session, 0x10 | ctrl, 0, point, 0, 0, 0, 0, p[0], p[1], p[2]]);
        };
        let audio: Vec<_> = toc.pistes_audio().collect();
        let (der, fin_audio) = toc.fin_audio();
        desc(1, 0, 0xA0, [audio[0].numero, 0, 0]);
        desc(1, 0, 0xA1, [der, 0, 0]);
        desc(1, 0, 0xA2, msf(fin_audio));
        for p in &audio {
            desc(1, 0, p.numero, msf(p.debut));
        }
        for p in toc.pistes.iter().filter(|p| !p.audio) {
            desc(2, 4, 0xA0, [p.numero, 0, 0]);
            desc(2, 4, 0xA1, [p.numero, 0, 0]);
            desc(2, 4, 0xA2, msf(toc.fin));
            desc(2, 4, p.numero, msf(p.debut));
        }
        let sessions = if toc.pistes.iter().any(|p| !p.audio) {
            2
        } else {
            1
        };
        let mut v = ((d.len() + 2) as u16).to_be_bytes().to_vec();
        v.extend([1, sessions]);
        v.extend(d);
        v
    }

    /// Un `.TOC.plist` à la manière de `cddafs` : la TOC brute ET `Sessions`.
    pub(crate) fn plist_de(toc: &Toc, avec_brute: bool, avec_sessions: bool) -> String {
        use base64::Engine;
        let mut s = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
        );
        if avec_brute {
            let b64 = base64::engine::general_purpose::STANDARD.encode(toc_brute(toc));
            s.push_str("\t<key>Format 0x02 TOC Data</key>\n\t<data>\n");
            for l in b64.as_bytes().chunks(68) {
                s.push_str(&format!("\t{}\n", std::str::from_utf8(l).unwrap()));
            }
            s.push_str("\t</data>\n");
        }
        if avec_sessions {
            s.push_str("\t<key>Sessions</key>\n\t<array>\n");
            let (_, fin_audio) = toc.fin_audio();
            let donnees: Vec<_> = toc.pistes.iter().filter(|p| !p.audio).collect();
            let mut sessions = vec![(1, toc.pistes_audio().collect::<Vec<_>>(), fin_audio)];
            if !donnees.is_empty() {
                sessions.push((2, donnees, toc.fin));
            }
            for (n, pistes, fin) in sessions {
                s.push_str(&format!(
                    "\t\t<dict>\n\t\t\t<key>First Track</key>\n\t\t\t<integer>{}</integer>\n\t\t\t<key>Last Track</key>\n\t\t\t<integer>{}</integer>\n\t\t\t<key>Leadout Block</key>\n\t\t\t<integer>{}</integer>\n\t\t\t<key>Session Number</key>\n\t\t\t<integer>{n}</integer>\n\t\t\t<key>Session Type</key>\n\t\t\t<integer>0</integer>\n\t\t\t<key>Track Array</key>\n\t\t\t<array>\n",
                    pistes[0].numero,
                    pistes.last().unwrap().numero,
                    fin + PREGAP
                ));
                for p in pistes {
                    s.push_str(&format!(
                        "\t\t\t\t<dict>\n\t\t\t\t\t<key>Data</key>\n\t\t\t\t\t<{}/>\n\t\t\t\t\t<key>Point</key>\n\t\t\t\t\t<integer>{}</integer>\n\t\t\t\t\t<key>Pre-Emphasis Enabled</key>\n\t\t\t\t\t<false/>\n\t\t\t\t\t<key>Session Number</key>\n\t\t\t\t\t<integer>{n}</integer>\n\t\t\t\t\t<key>Start Block</key>\n\t\t\t\t\t<integer>{}</integer>\n\t\t\t\t</dict>\n",
                        if p.audio { "false" } else { "true" },
                        p.numero,
                        p.debut + PREGAP
                    ));
                }
                s.push_str("\t\t\t</array>\n\t\t</dict>\n");
            }
            s.push_str("\t</array>\n");
        }
        s.push_str("</dict>\n</plist>\n");
        s
    }

    /// Un faux volume `cddafs` : `.TOC.plist` et un AIFF par piste audio.
    pub(crate) fn faux_volume(nom: &str, toc: &Toc) -> Dossier {
        let d = Dossier::nouveau(nom);
        std::fs::write(d.0.join(FICHIER_TOC), plist_de(toc, true, true)).unwrap();
        for p in toc.pistes_audio() {
            let fin = toc.fin_de_piste(p.numero).unwrap();
            // Pistes paires en AIFC `sowt`, impaires en AIFF : les deux ordres
            // d'octets passent par le même lecteur.
            let sowt = p.numero % 2 == 0;
            std::fs::write(
                d.0.join(format!("{} Audio Track.aiff", p.numero)),
                aiff(p.debut, fin, sowt, p.numero as u32 * 3),
            )
            .unwrap();
        }
        d
    }

    /// Une petite TOC : 3 pistes, la première à 32 secteurs du début.
    pub(crate) fn petite_toc() -> Toc {
        let p = |numero, debut| PisteToc {
            numero,
            debut,
            audio: true,
        };
        Toc::nouvelle(vec![p(1, 32), p(2, 140), p(3, 331)], 420).unwrap()
    }

    #[test]
    fn le_plist_xml_se_lit_avec_ses_types() {
        let v = plist::lire(
            br#"<?xml version="1.0"?><!-- c --><plist version="1.0"><dict>
            <key>a &amp; b</key><integer>-12</integer>
            <key>t</key><true/><key>f</key><false/>
            <key>s</key><string>x&lt;y</string><key>vide</key><string/>
            <key>r</key><real>1.5</real>
            <key>d</key><data> AAEC
              /w== </data>
            <key>l</key><array><integer>1</integer><dict/></array>
            </dict></plist>"#,
        )
        .unwrap();
        assert_eq!(v.cle("a & b"), Some(&plist::Valeur::Entier(-12)));
        assert_eq!(v.cle("t").and_then(|v| v.booleen()), Some(true));
        assert_eq!(v.cle("f").and_then(|v| v.booleen()), Some(false));
        assert_eq!(v.cle("s"), Some(&plist::Valeur::Texte("x<y".into())));
        assert_eq!(v.cle("vide"), Some(&plist::Valeur::Texte(String::new())));
        assert_eq!(v.cle("r"), Some(&plist::Valeur::Reel(1.5)));
        assert_eq!(
            v.cle("d"),
            Some(&plist::Valeur::Donnees(vec![0, 1, 2, 0xFF]))
        );
        assert_eq!(
            v.cle("l").and_then(|l| l.tableau()).map(|l| l.len()),
            Some(2)
        );
        assert!(
            plist::lire(b"bplist00\x00")
                .unwrap_err()
                .contains("binaire")
        );
        assert!(plist::lire(b"<plist><dict><key>x</key></dict></plist>").is_err());
    }

    /// Les deux voies du plist — TOC brute (MSF) et `Sessions` (LBA) —
    /// rendent la même TOC, et le même identifiant que le vecteur libdiscid.
    #[test]
    fn le_toc_plist_donne_la_toc_et_l_identifiant_musicbrainz() {
        let toc = crate::discid::tests::toc_du_vecteur();
        for (brute, sessions) in [(true, true), (true, false), (false, true)] {
            let lue = toc_du_plist(plist_de(&toc, brute, sessions).as_bytes()).unwrap();
            assert_eq!(lue, toc, "brute={brute} sessions={sessions}");
            assert_eq!(disc_id(&lue), crate::discid::tests::ATTENDU);
        }
        assert!(toc_du_plist(b"<plist><dict/></plist>").is_err());
    }

    /// Un CD enrichi : la piste de données de la session 2 est vue comme
    /// telle, et le lead-out retenu est celui de la DERNIÈRE session.
    #[test]
    fn un_cd_enrichi_garde_sa_piste_de_donnees_et_son_dernier_lead_out() {
        let p = |numero, debut, audio| PisteToc {
            numero,
            debut,
            audio,
        };
        let toc = Toc::nouvelle(
            vec![p(1, 0, true), p(2, 20_000, true), p(3, 60_000, false)],
            90_000,
        )
        .unwrap();
        for (brute, sessions) in [(true, false), (false, true)] {
            let lue = toc_du_plist(plist_de(&toc, brute, sessions).as_bytes()).unwrap();
            assert_eq!(lue, toc, "brute={brute}");
            assert_eq!(lue.fin_audio(), (2, 48_600));
        }
    }

    /// Le `.TOC.plist` d'un VRAI disque (Apple SuperDrive, macOS, 25/09/2026 ;
    /// 8 pistes, 51:22:33). Ses deux voies donnent la même TOC — piste 1 au
    /// LBA 75 : `Start Block` = 225 est une adresse ABSOLUE (00:03:00) — et
    /// l'identifiant que MusicBrainz connaît pour ce disque.
    #[test]
    fn le_toc_plist_d_un_vrai_disque_donne_l_identifiant_connu_de_musicbrainz() {
        let o = include_bytes!("../tests/fixtures/toc_superdrive_20260925.plist");
        let racine = plist::lire(o).unwrap();
        let brute = match racine.cle(CLE_TOC_BRUTE) {
            Some(plist::Valeur::Donnees(d)) => toc_de_la_toc_brute(d).unwrap(),
            autre => panic!("TOC brute absente : {autre:?}"),
        };
        let sessions = toc_des_sessions(&racine).unwrap();
        assert_eq!(
            sessions.pistes.iter().map(|p| p.debut).collect::<Vec<_>>(),
            vec![
                75, 26_897, 55_952, 85_235, 119_382, 147_798, 184_208, 204_840
            ],
            "Start Block est absolu (LBA + 150)"
        );
        assert_eq!(sessions.fin, 231_258);
        assert_eq!(brute, sessions, "les deux voies du plist divergent");
        assert_eq!(toc_du_plist(o).unwrap(), brute);
        // https://musicbrainz.org/ws/2/discid/xA6kQVeYfzh._3s2lYEibSII2pg- :
        // offsets [225, 27047, …], 231408 secteurs — disque connu.
        assert_eq!(disc_id(&sessions), "xA6kQVeYfzh._3s2lYEibSII2pg-");
    }

    #[test]
    fn l_en_tete_aiff_se_lit_sans_taille_supposee() {
        for (sowt, offset) in [(false, 0), (false, 7), (true, 4)] {
            let f = aiff(10, 13, sowt, offset);
            let e = entete_aiff(&f).unwrap();
            assert_eq!((e.canaux, e.bits, e.frequence), (2, 16, 44_100));
            assert_eq!(e.trames, 3 * 588);
            assert_eq!(e.petit_boutiste, sowt);
            // Le PCM commence bien là où l'en-tête le dit.
            let mut pcm = contenu_des_secteurs(10, 3);
            if !sowt {
                echanger_octets(&mut pcm);
            }
            let d = e.debut_donnees as usize;
            assert_eq!(&f[d..d + pcm.len()], &pcm[..]);
        }
        assert!(entete_aiff(b"RIFF....WAVE").is_err());
        let mut sans_ssnd = aiff(0, 1, false, 0);
        let p = sans_ssnd.windows(4).position(|w| w == b"SSND").unwrap();
        sans_ssnd[p..p + 4].copy_from_slice(b"XXXX");
        assert!(entete_aiff(&sans_ssnd).unwrap_err().contains("SSND"));
    }

    #[test]
    fn le_nom_du_fichier_donne_le_numero_de_piste() {
        assert_eq!(numero_du_fichier("1 Audio Track.aiff"), Some(1));
        assert_eq!(numero_du_fichier("12 Audio Track.aiff"), Some(12));
        assert_eq!(numero_du_fichier("Piste audio 7.AIFF"), Some(7));
        assert_eq!(numero_du_fichier(".TOC.plist"), None);
        assert_eq!(numero_du_fichier("._1 Audio Track.aiff"), None);
        assert_eq!(numero_du_fichier("notes.txt"), None);
    }

    #[test]
    fn le_plan_de_lecture_coupe_a_la_jonction_des_pistes() {
        let f = |numero, debut, secteurs, debut_donnees| FichierPiste {
            numero,
            chemin: PathBuf::new(),
            debut,
            secteurs,
            debut_donnees,
            petit_boutiste: false,
        };
        let fichiers = [f(1, 32, 108, 54), f(2, 140, 191, 70)];
        assert_eq!(
            plan_de_lecture(&fichiers, 32, 2),
            Ok(vec![Morceau {
                fichier: 0,
                position: 54,
                octets: 2 * 2352
            }])
        );
        assert_eq!(
            plan_de_lecture(&fichiers, 138, 4),
            Ok(vec![
                Morceau {
                    fichier: 0,
                    position: 54 + 106 * 2352,
                    octets: 2 * 2352
                },
                Morceau {
                    fichier: 1,
                    position: 70,
                    octets: 2 * 2352
                },
            ])
        );
        assert_eq!(plan_de_lecture(&fichiers, 31, 2), Err(31));
        assert_eq!(plan_de_lecture(&fichiers, 330, 2), Err(331));
    }

    /// Le cœur : le lecteur sur un faux volume rend EXACTEMENT les secteurs
    /// bruts (petit-boutistes) que rendrait le lecteur Linux, y compris à
    /// cheval sur deux pistes d'ordres d'octets différents.
    #[test]
    fn le_lecteur_de_volume_rend_les_secteurs_bruts_a_travers_les_pistes() {
        let toc = petite_toc();
        let v = faux_volume("lecture", &toc);
        let l = LecteurVolume::sur_dossier(v.0.clone());
        assert_eq!(l.presence(), Presence::Disque);
        assert_eq!(l.lire_toc().unwrap(), toc);
        for (lba, n) in [(32, 1), (32, 24), (130, 24), (320, 20), (400, 20)] {
            let mut s = vec![0; n as usize * OCTETS_PAR_SECTEUR];
            l.lire_secteurs(lba, n, &mut s).unwrap();
            assert!(s == contenu_des_secteurs(lba, n), "lba {lba} n {n}");
        }
        // Avant la première piste, et après la fin : erreur de lecture.
        let mut s = vec![0; OCTETS_PAR_SECTEUR];
        assert!(matches!(
            l.lire_secteurs(0, 1, &mut s),
            Err(ErreurCd::Lecture { lba: 0, .. })
        ));
        assert!(matches!(
            l.lire_secteurs(420, 1, &mut s),
            Err(ErreurCd::Lecture { .. })
        ));
        let mut faux = vec![0; 3];
        assert!(l.lire_secteurs(32, 1, &mut faux).is_err());
    }

    /// Une piste entière, par le fournisseur PCM et le flux du greffon :
    /// l'identifiant de disque, la plage et chaque octet.
    #[test]
    fn une_piste_se_joue_de_bout_en_bout_par_le_fournisseur() {
        use crate::fournisseur::{FournisseurCd, source_id};
        use tune_core::source_pcm::FournisseurPcm;
        let toc = petite_toc();
        let v = faux_volume("fournisseur", &toc);
        let l: Arc<dyn LecteurDisque> = Arc::new(LecteurVolume::sur_dossier(v.0.clone()));
        let f = FournisseurCd { lecteur: l };
        let mut flux = f.ouvrir(&source_id(&disc_id(&toc), 2), 0).unwrap();
        assert_eq!(flux.octets, 191 * OCTETS_PAR_SECTEUR as u64);
        let mut tout = Vec::new();
        flux.lecteur.read_to_end(&mut tout).unwrap();
        assert!(tout == contenu_des_secteurs(140, 191));
    }

    /// Sans `.TOC.plist`, la TOC se déduit des fichiers (bout à bout depuis 0).
    #[test]
    fn sans_toc_plist_la_toc_se_deduit_des_fichiers() {
        let toc = petite_toc();
        let v = faux_volume("sans-plist", &toc);
        std::fs::remove_file(v.0.join(FICHIER_TOC)).unwrap();
        let vol = VolumeCdda::ouvrir(&v.0).unwrap();
        assert_eq!(
            vol.toc.pistes.iter().map(|p| p.debut).collect::<Vec<_>>(),
            vec![0, 108, 299]
        );
        assert_eq!(vol.toc.fin, 388);
    }

    /// Éjection = volume disparu : la lecture le dit `AucunDisque` (le flux
    /// s'arrête), la présence passe à « vide ».
    #[test]
    fn l_ejection_est_la_disparition_du_volume() {
        let toc = petite_toc();
        let v = faux_volume("ejection", &toc);
        let racine = v.0.clone();
        let l = LecteurVolume::new(
            "lecteur optique",
            move || racine.join(FICHIER_TOC).exists().then(|| racine.clone()),
            || true,
        );
        let mut s = vec![0; 24 * OCTETS_PAR_SECTEUR];
        l.lire_secteurs(32, 24, &mut s).unwrap();
        std::fs::remove_dir_all(&v.0).unwrap();
        assert_eq!(l.presence(), Presence::Vide);
        assert_eq!(l.chemin(), "lecteur optique");
        assert_eq!(l.lire_secteurs(56, 24, &mut s), Err(ErreurCd::AucunDisque));
        assert_eq!(l.lire_toc(), Err(ErreurCd::AucunDisque));
        let sans_lecteur = LecteurVolume::new("x", || None, || false);
        assert_eq!(sans_lecteur.presence(), Presence::AucunLecteur);
    }

    /// Un fichier devenu illisible (secteur rayé) est une erreur de LECTURE,
    /// que le flux rejoue puis remplace par du silence — pas une éjection.
    #[test]
    fn un_fichier_illisible_est_une_erreur_de_lecture_pas_une_ejection() {
        let toc = petite_toc();
        let v = faux_volume("rayure", &toc);
        let l = LecteurVolume::sur_dossier(v.0.clone());
        l.lire_toc().unwrap();
        // Le fichier de la piste 3 est tronqué : ses derniers secteurs manquent.
        let p3 = v.0.join("3 Audio Track.aiff");
        let o = std::fs::read(&p3).unwrap();
        std::fs::write(&p3, &o[..o.len() - 10 * OCTETS_PAR_SECTEUR]).unwrap();
        let mut s = vec![0; 2 * OCTETS_PAR_SECTEUR];
        assert!(matches!(
            l.lire_secteurs(415, 2, &mut s),
            Err(ErreurCd::Lecture { .. })
        ));
        assert_eq!(l.presence(), Presence::Disque);
        // Le flux, lui, rend la piste entière, les secteurs perdus en silence.
        let mut f = crate::flux::FluxPiste::new(Arc::new(l), 400, 420);
        let mut tout = Vec::new();
        f.read_to_end(&mut tout).unwrap();
        assert_eq!(tout.len(), 20 * OCTETS_PAR_SECTEUR);
        assert_eq!(f.secteurs_perdus, 10);
        assert!(tout[..10 * OCTETS_PAR_SECTEUR] == contenu_des_secteurs(400, 10));
        assert!(tout[10 * OCTETS_PAR_SECTEUR..].iter().all(|&b| b == 0));
    }
}
