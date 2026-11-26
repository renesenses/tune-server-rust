//! TESS T09a — import d'un jeu de réponses impulsionnelles pour la
//! convolution : un `.wav` seul, un `.zip` de `.wav`, ou un `.zip` de fichiers
//! de configuration `.cfg` « Convolver » avec leurs réponses.
//!
//! Le résultat est un jeu de réponses, UNE PAR CADENCE : la sortie choisit
//! ensuite automatiquement le fichier qui correspond à la cadence de la
//! source, ou rééchantillonne le plus proche (#8761, `convolver::par_cadence`).
//!
//! # Sécurité de la décompression
//!
//! Une archive vient d'un utilisateur ou d'un site tiers : elle est traitée
//! comme hostile. Rien n'est jamais écrit sur disque sous un nom venu de
//! l'archive ; tout se lit en mémoire, sous des plafonds vérifiés sur les
//! octets RÉELLEMENT décompressés (pas sur la taille annoncée, qui peut
//! mentir) :
//!
//! - archive de [`ARCHIVE_MAX_OCTETS`] au plus, [`ENTREES_MAX`] entrées au
//!   plus ;
//! - chaque fichier lu décompresse au plus [`ENTREE_MAX_OCTETS`], le total au
//!   plus [`TOTAL_MAX_OCTETS`] (bombe de décompression) ; un rapport de
//!   compression au-delà de [`RAPPORT_MAX`] est refusé ;
//! - un chemin absolu, avec `..`, ou que la bibliothèque ne sait pas
//!   enfermer, refuse TOUTE l'archive (`unsafe_path`), de même qu'un lien
//!   symbolique (`symlink`) ou une entrée chiffrée (`encrypted`) ;
//! - les noms référencés par un `.cfg` sont résolus DANS l'archive, avec les
//!   mêmes règles.
//!
//! # Le format `.cfg` « Convolver »
//!
//! ```text
//! 96000 2 2 0            cadence, canaux d'entrée, canaux de sortie, partition
//! 0 0                    retards d'entrée (ms), un par canal
//! 0 0                    retards de sortie (ms), un par canal
//! room_96k.wav           puis, pour chaque chemin : le fichier,
//! 0                      le canal du fichier,
//! 0.0                    le(s) canal(aux) d'entrée « canal.atténuation »,
//! 0.0                    le(s) canal(aux) de sortie « canal.atténuation »
//! room_96k.wav
//! 1
//! 1.0
//! 1.0
//! ```
//!
//! Tune applique un filtre PAR CANAL : chaque sortie reçoit exactement un
//! chemin, depuis l'entrée de même numéro. Un mélange entre canaux (matrice)
//! est refusé avec un motif (`matrix_routing`) plutôt que joué faux. Les
//! retards sont appliqués en tête de la réponse du canal (arrondis à
//! l'échantillon) ; une atténuation non nulle est refusée (`invalid_cfg`).

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use super::convolver::Convolver;
use super::convolver::par_cadence;

/// Taille maximale d'une archive déposée : la limite du corps des requêtes
/// du serveur (`DefaultBodyLimit`, 50 Mio).
pub const ARCHIVE_MAX_OCTETS: usize = 50 << 20;
/// Nombre maximal d'entrées d'une archive (dossiers compris).
pub const ENTREES_MAX: usize = 512;
/// Octets décompressés au plus par fichier lu.
pub const ENTREE_MAX_OCTETS: u64 = 48 << 20;
/// Octets décompressés au plus pour toute l'archive : sous la limite du
/// corps, la mémoire d'un import reste du même ordre que celle de la requête.
pub const TOTAL_MAX_OCTETS: u64 = 48 << 20;
/// Rapport décompressé / taille RÉELLE de l'archive au-delà duquel elle est
/// refusée (au-dessus de 1 Mio décompressé). Mesuré sur les octets lus et sur
/// la longueur du dépôt, jamais sur les tailles que l'archive annonce.
pub const RAPPORT_MAX: u64 = 100;
/// Réponses au plus dans un jeu (une par cadence).
pub const REPONSES_MAX: usize = par_cadence::CADENCES_USUELLES.len() + 2;
/// Somme canaux × prises au plus pour TOUT le jeu (16 Mi échantillons, 64 Mio
/// en `f32`) : borne la mémoire décodée, quel que soit le nombre de chemins
/// d'un `.cfg`.
pub const ECHANTILLONS_MAX: usize = 1 << 24;

/// Taille maximale d'un `.cfg`.
const CFG_MAX_OCTETS: u64 = 64 << 10;

/// Pourquoi une archive est refusée. Le code (`raison`) est stable : l'API
/// le renvoie (`reason`) et l'écran le traduit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusImport {
    pub raison: &'static str,
    pub detail: String,
}

impl RefusImport {
    fn new(raison: &'static str, detail: impl Into<String>) -> Self {
        Self {
            raison,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for RefusImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.detail, self.raison)
    }
}

/// D'où vient une réponse du jeu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origine {
    /// Un `.wav` de l'archive (ou le `.wav` déposé seul).
    Wav,
    /// Deux `.wav` mono gauche et droite réunis.
    PaireMono,
    /// Un `.cfg` Convolver et ses réponses.
    Cfg,
}

/// Une réponse du jeu, prête à être écrite (une par cadence).
#[derive(Debug, Clone)]
pub struct RiImportee {
    /// Le nom d'origine (fichier ou `.cfg`).
    pub nom: String,
    pub sample_rate: u32,
    /// Une réponse par canal.
    pub taps: Vec<Vec<f32>>,
    pub origine: Origine,
}

/// Le jeu importé, et ce qui a été écarté (avec la raison).
#[derive(Debug, Clone, Default)]
pub struct JeuImporte {
    /// Triées par cadence croissante.
    pub reponses: Vec<RiImportee>,
    pub ecartes: Vec<(String, String)>,
}

impl JeuImporte {
    /// L'indice de la réponse à installer comme fichier PRINCIPAL : 48 kHz
    /// s'il existe, sinon 44,1 kHz, sinon la plus haute cadence.
    pub fn indice_principal(&self) -> Option<usize> {
        let p = |sr| self.reponses.iter().position(|r| r.sample_rate == sr);
        p(48_000)
            .or_else(|| p(44_100))
            .or_else(|| self.reponses.len().checked_sub(1))
    }
}

/// rc14 (Didier) — clé i18n de l'avertissement « ce filtre amplifie » :
/// l'écran la traduit avec les paramètres bruts de [`AvertissementDeGain`].
pub const CLE_GAIN_POSITIF: &str = "api.roomCorrection.import.filter_gain_positive";

/// rc14 (Didier) — un filtre du jeu qui AMPLIFIE (gain maximal au-dessus de
/// 0 dB) : il est importé tel quel, l'utilisateur est prévenu. La marge est
/// celle que le moteur appliquera en Auto à la cadence du fichier.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AvertissementDeGain {
    /// Code stable : `filter_gain_positive`.
    pub code: &'static str,
    /// [`CLE_GAIN_POSITIF`].
    pub message_key: &'static str,
    /// Le nom d'origine de la réponse.
    pub name: String,
    pub sample_rate: u32,
    /// `max |H(f)|`, dB, pire canal.
    pub gain_max_db: f64,
    /// La marge automatique correspondante (dB, ≤ 0).
    pub headroom_db: f64,
}

/// rc14 (Didier) — les avertissements de gain d'un jeu importé, une FFT par
/// réponse et par canal (hors temps réel). Le filtre n'est JAMAIS modifié.
pub fn avertissements_de_gain(jeu: &JeuImporte) -> Vec<AvertissementDeGain> {
    jeu.reponses
        .iter()
        .filter_map(|r| {
            let g = super::convolver::courbe_fft::gain_max_db(&r.taps, r.sample_rate)?;
            let marge = crate::tess::etages_fixes::marge_de_convolution(
                &crate::tess::chaine::Headroom::Auto {},
                g,
                None,
            );
            (marge.requise_db < 0.0).then(|| AvertissementDeGain {
                code: "filter_gain_positive",
                message_key: CLE_GAIN_POSITIF,
                name: r.nom.clone(),
                sample_rate: r.sample_rate,
                gain_max_db: (g * 100.0).round() / 100.0,
                headroom_db: (marge.requise_db * 100.0).round() / 100.0,
            })
        })
        .collect()
}

/// `true` si les octets commencent comme une archive zip.
pub fn est_une_archive_zip(octets: &[u8]) -> bool {
    octets.starts_with(b"PK\x03\x04") || octets.starts_with(b"PK\x05\x06")
}

/// Importer un dépôt : un `.zip` (reconnu à sa signature) ou un `.wav` seul.
pub fn importer(octets: &[u8], nom: &str) -> Result<JeuImporte, RefusImport> {
    if est_une_archive_zip(octets) {
        return importer_archive(octets);
    }
    let (taps, sr) = lire_wav(octets, nom)?;
    Ok(JeuImporte {
        reponses: vec![RiImportee {
            nom: nom.to_string(),
            sample_rate: sr,
            taps,
            origine: Origine::Wav,
        }],
        ecartes: Vec::new(),
    })
}

fn lire_wav(octets: &[u8], nom: &str) -> Result<(Vec<Vec<f32>>, u32), RefusImport> {
    let (taps, sr) = Convolver::lire_ri_wav(octets, nom)
        .map_err(|e| RefusImport::new("bad_wav", format!("{nom} : {e}")))?;
    par_cadence::ri_admise(&taps, sr)
        .map_err(|e| RefusImport::new("bad_wav", format!("{nom} : {e}")))?;
    Ok((taps, sr))
}

/// Le chemin NORMALISÉ d'une entrée (séparateurs `/`, sans `.`), ou `None`
/// s'il sort de l'archive (absolu, `..`, lecteur Windows).
fn chemin_sur(nom: &str) -> Option<String> {
    let nom = nom.replace('\\', "/");
    if nom.starts_with('/') || nom.contains(':') || nom.contains('\0') {
        return None;
    }
    let mut parties = Vec::new();
    for p in nom.split('/') {
        match p {
            "" | "." => {}
            ".." => return None,
            p => parties.push(p),
        }
    }
    (!parties.is_empty()).then(|| parties.join("/"))
}

/// Une entrée à ignorer sans la lire : dossiers système et fichiers cachés.
fn ignoree(chemin: &str) -> bool {
    chemin
        .split('/')
        .any(|p| p.starts_with('.') || p == "__MACOSX")
}

fn extension(chemin: &str) -> String {
    chemin
        .rsplit('/')
        .next()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Lire toutes les entrées utiles d'une archive, sous les plafonds.
fn lire_les_entrees(octets: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, RefusImport> {
    if octets.len() > ARCHIVE_MAX_OCTETS {
        return Err(RefusImport::new(
            "too_large",
            format!(
                "archive de {} octets (au plus {ARCHIVE_MAX_OCTETS})",
                octets.len()
            ),
        ));
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(octets))
        .map_err(|e| RefusImport::new("not_a_zip", format!("archive illisible : {e}")))?;
    if zip.len() > ENTREES_MAX {
        return Err(RefusImport::new(
            "too_many_entries",
            format!("{} entrées (au plus {ENTREES_MAX})", zip.len()),
        ));
    }
    // Premier passage, SANS décompresser : chemins, liens, chiffrement.
    let mut a_lire = Vec::new();
    for i in 0..zip.len() {
        let entree = zip
            .by_index_raw(i)
            .map_err(|e| RefusImport::new("not_a_zip", format!("entrée {i} illisible : {e}")))?;
        let nom = entree
            .name()
            .map(|n| n.to_string())
            .map_err(|e| RefusImport::new("not_a_zip", format!("nom d'entrée illisible : {e}")))?;
        let Some(chemin) = chemin_sur(&nom).filter(|_| entree.enclosed_name().is_some()) else {
            return Err(RefusImport::new(
                "unsafe_path",
                format!("chemin hors de l'archive : {nom}"),
            ));
        };
        if entree.is_symlink() {
            return Err(RefusImport::new(
                "symlink",
                format!("lien symbolique : {nom}"),
            ));
        }
        if entree.encrypted() {
            return Err(RefusImport::new(
                "encrypted",
                format!("entrée chiffrée : {nom}"),
            ));
        }
        if entree.is_dir() || ignoree(&chemin) {
            continue;
        }
        if matches!(extension(&chemin).as_str(), "wav" | "cfg") {
            a_lire.push((i, chemin));
        }
    }
    // Second passage : décompresser sous plafond RÉEL.
    let longueur = octets.len().max(1) as u64;
    let mut total = 0u64;
    let mut lus = BTreeMap::new();
    for (i, chemin) in a_lire {
        let plafond = if extension(&chemin) == "cfg" {
            CFG_MAX_OCTETS
        } else {
            ENTREE_MAX_OCTETS
        };
        let restant = TOTAL_MAX_OCTETS.saturating_sub(total);
        let entree = zip
            .by_index(i)
            .map_err(|e| RefusImport::new("not_a_zip", format!("{chemin} : {e}")))?;
        let mut contenu = Vec::new();
        entree
            .take(plafond.min(restant) + 1)
            .read_to_end(&mut contenu)
            .map_err(|e| RefusImport::new("not_a_zip", format!("{chemin} : {e}")))?;
        let n = contenu.len() as u64;
        if n > plafond {
            return Err(RefusImport::new(
                "too_large",
                format!("{chemin} : plus de {plafond} octets décompressés"),
            ));
        }
        total += n;
        if total > TOTAL_MAX_OCTETS {
            return Err(RefusImport::new(
                "too_large",
                format!("plus de {TOTAL_MAX_OCTETS} octets décompressés au total"),
            ));
        }
        if total > (1 << 20) && total > longueur.saturating_mul(RAPPORT_MAX) {
            return Err(RefusImport::new(
                "compression_ratio",
                format!(
                    "rapport de compression de {} (au plus {RAPPORT_MAX})",
                    total / longueur
                ),
            ));
        }
        if lus.insert(chemin.to_ascii_lowercase(), contenu).is_some() {
            return Err(RefusImport::new(
                "unsafe_path",
                format!("deux entrées portent le même nom : {chemin}"),
            ));
        }
    }
    Ok(lus)
}

/// Importer une archive : mode `.cfg` dès qu'elle en contient un, sinon un
/// jeu de `.wav`.
pub fn importer_archive(octets: &[u8]) -> Result<JeuImporte, RefusImport> {
    let entrees = lire_les_entrees(octets)?;
    let a_des_cfg = entrees.keys().any(|k| extension(k) == "cfg");
    let mut jeu = if a_des_cfg {
        importer_cfg(&entrees)?
    } else {
        importer_wav(&entrees)?
    };
    if jeu.reponses.is_empty() {
        return Err(RefusImport::new(
            "no_filter",
            "l'archive ne contient aucune réponse impulsionnelle utilisable",
        ));
    }
    if jeu.reponses.len() > REPONSES_MAX {
        return Err(RefusImport::new(
            "too_many_entries",
            format!("{} réponses (au plus {REPONSES_MAX})", jeu.reponses.len()),
        ));
    }
    jeu.reponses.sort_by_key(|r| r.sample_rate);
    Ok(jeu)
}

/// Le côté (gauche 0, droite 1) que le NOM d'un fichier mono désigne :
/// `filter_L.wav`, `room-right-48k.wav`, `R_44100.wav`…
fn cote_du_nom(chemin: &str) -> Option<usize> {
    let base = chemin.rsplit('/').next().unwrap_or(chemin);
    let tige = base
        .rsplit_once('.')
        .map_or(base, |(t, _)| t)
        .to_lowercase();
    let mut cote = None;
    // Jetons Unicode : « réponse » reste un seul mot (un découpage ASCII en
    // ferait « r » + « ponse », lu comme « droite »).
    for jeton in tige.split(|c: char| !c.is_alphanumeric()) {
        let c = match jeton {
            "l" | "left" | "gauche" | "g" => Some(0),
            "r" | "right" | "droite" | "d" => Some(1),
            _ => None,
        };
        if c.is_some() {
            if cote.is_some() && cote != c {
                return None;
            }
            cote = c;
        }
    }
    cote
}

/// Retire `n` échantillons du budget du jeu ([`ECHANTILLONS_MAX`]), ou refuse.
fn consommer(budget: &mut usize, n: usize, nom: &str) -> Result<(), RefusImport> {
    if n > *budget {
        return Err(RefusImport::new(
            "too_large",
            format!("{nom} : le jeu dépasse {ECHANTILLONS_MAX} échantillons"),
        ));
    }
    *budget -= n;
    Ok(())
}

/// Le nombre d'échantillons (canaux × prises) d'une réponse.
fn echantillons(taps: &[Vec<f32>]) -> usize {
    taps.len() * taps.iter().map(Vec::len).max().unwrap_or(0)
}

fn importer_wav(entrees: &BTreeMap<String, Vec<u8>>) -> Result<JeuImporte, RefusImport> {
    let mut jeu = JeuImporte::default();
    // Par cadence : les fichiers lus.
    let mut par_cadence: BTreeMap<u32, Vec<(String, Vec<Vec<f32>>)>> = BTreeMap::new();
    let mut budget = ECHANTILLONS_MAX;
    for (chemin, octets) in entrees {
        match lire_wav(octets, chemin) {
            Ok((taps, sr)) => {
                consommer(&mut budget, echantillons(&taps), chemin)?;
                par_cadence
                    .entry(sr)
                    .or_default()
                    .push((chemin.clone(), taps));
            }
            Err(e) => jeu.ecartes.push((chemin.clone(), e.detail)),
        }
    }
    for (sr, mut fichiers) in par_cadence {
        if fichiers.len() == 1 {
            let (nom, taps) = fichiers.remove(0);
            jeu.reponses.push(RiImportee {
                nom,
                sample_rate: sr,
                taps,
                origine: Origine::Wav,
            });
            continue;
        }
        // Deux mono, un gauche et un droit (export REW, Acourate…) : un stéréo.
        if fichiers.len() == 2 && fichiers.iter().all(|(_, t)| t.len() == 1) {
            let cotes: Vec<_> = fichiers.iter().map(|(n, _)| cote_du_nom(n)).collect();
            if let [Some(a), Some(b)] = cotes[..]
                && a != b
            {
                if a == 1 {
                    fichiers.swap(0, 1);
                }
                let nom = format!("{} + {}", fichiers[0].0, fichiers[1].0);
                let mut d = std::mem::take(&mut fichiers[1].1[0]);
                let mut g = std::mem::take(&mut fichiers[0].1[0]);
                let n = g.len().max(d.len());
                g.resize(n, 0.0);
                d.resize(n, 0.0);
                jeu.reponses.push(RiImportee {
                    nom,
                    sample_rate: sr,
                    taps: vec![g, d],
                    origine: Origine::PaireMono,
                });
                continue;
            }
        }
        let noms: Vec<_> = fichiers.iter().map(|(n, _)| n.as_str()).collect();
        return Err(RefusImport::new(
            "duplicate_rate",
            format!("plusieurs fichiers à {sr} Hz : {}", noms.join(", ")),
        ));
    }
    Ok(jeu)
}

/// Un chemin de filtrage d'un `.cfg`.
struct CheminCfg {
    fichier: String,
    canal_fichier: usize,
    entrees: Vec<usize>,
    sorties: Vec<usize>,
}

/// Une liste « canal.atténuation » d'un `.cfg`.
fn canaux_cfg(ligne: &str, nom: &str) -> Result<Vec<usize>, RefusImport> {
    let mut canaux = Vec::new();
    for jeton in ligne.split_whitespace() {
        let (c, att) = jeton.split_once('.').unwrap_or((jeton, "0"));
        let c: usize = c.parse().map_err(|_| {
            RefusImport::new(
                "invalid_cfg",
                format!("{nom} : canal illisible « {jeton} »"),
            )
        })?;
        if att.trim_end_matches('0') != "" {
            return Err(RefusImport::new(
                "invalid_cfg",
                format!("{nom} : atténuation par canal non prise en charge (« {jeton} »)"),
            ));
        }
        canaux.push(c);
    }
    Ok(canaux)
}

fn retards_cfg(ligne: &str, n: usize, nom: &str) -> Result<Vec<f64>, RefusImport> {
    let v: Vec<f64> = ligne
        .split_whitespace()
        .map(|t| t.parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| {
            RefusImport::new(
                "invalid_cfg",
                format!("{nom} : retards illisibles « {ligne} »"),
            )
        })?;
    if v.len() != n || v.iter().any(|r| !r.is_finite() || *r < 0.0 || *r > 1000.0) {
        return Err(RefusImport::new(
            "invalid_cfg",
            format!("{nom} : {n} retards entre 0 et 1000 ms attendus, lu « {ligne} »"),
        ));
    }
    Ok(v)
}

/// Lire un `.cfg` Convolver et bâtir sa réponse multicanal.
/// Les `.wav` d'une archive `.cfg`, décodés UNE fois chacun (plusieurs
/// chemins citent souvent le même fichier), sous le budget du jeu.
type Decodes = BTreeMap<String, (Vec<Vec<f32>>, u32)>;

fn lire_cfg(
    nom: &str,
    texte: &str,
    entrees: &BTreeMap<String, Vec<u8>>,
    utilises: &mut Vec<String>,
    decodes: &mut Decodes,
    budget: &mut usize,
) -> Result<RiImportee, RefusImport> {
    let lignes: Vec<&str> = texte
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lignes.len() < 3 {
        return Err(RefusImport::new(
            "invalid_cfg",
            format!("{nom} : en-tête incomplet"),
        ));
    }
    let tete: Vec<&str> = lignes[0].split_whitespace().collect();
    let lire = |i: usize| -> Result<u64, RefusImport> {
        tete.get(i)
            .and_then(|t| t.parse::<u64>().ok())
            .ok_or_else(|| {
                RefusImport::new(
                    "invalid_cfg",
                    format!("{nom} : en-tête illisible « {} »", lignes[0]),
                )
            })
    };
    let sr = u32::try_from(lire(0)?).unwrap_or(0);
    let (n_in, n_out) = (lire(1)? as usize, lire(2)? as usize);
    if !par_cadence::CADENCES_USUELLES.contains(&sr) {
        return Err(RefusImport::new(
            "invalid_cfg",
            format!("{nom} : cadence de {sr} Hz (cadences admises : 44,1 à 192 kHz)"),
        ));
    }
    if n_in == 0 || n_out == 0 || n_in > par_cadence::CANAUX_MAX || n_out > par_cadence::CANAUX_MAX
    {
        return Err(RefusImport::new(
            "invalid_cfg",
            format!("{nom} : {n_in} entrées et {n_out} sorties"),
        ));
    }
    if n_in != n_out {
        return Err(RefusImport::new(
            "matrix_routing",
            format!("{nom} : {n_in} entrées pour {n_out} sorties (un filtre par canal attendu)"),
        ));
    }
    let retard_entree = retards_cfg(lignes[1], n_in, nom)?;
    let retard_sortie = retards_cfg(lignes[2], n_out, nom)?;
    let corps = &lignes[3..];
    if corps.is_empty() || corps.len() % 4 != 0 {
        return Err(RefusImport::new(
            "invalid_cfg",
            format!("{nom} : chemins incomplets (4 lignes par chemin)"),
        ));
    }
    let mut chemins = Vec::new();
    for bloc in corps.chunks(4) {
        chemins.push(CheminCfg {
            fichier: bloc[0].to_string(),
            canal_fichier: bloc[1].parse().map_err(|_| {
                RefusImport::new(
                    "invalid_cfg",
                    format!("{nom} : canal de fichier illisible « {} »", bloc[1]),
                )
            })?,
            entrees: canaux_cfg(bloc[2], nom)?,
            sorties: canaux_cfg(bloc[3], nom)?,
        });
    }
    // Un filtre par canal : chaque sortie reçoit exactement un chemin, depuis
    // l'entrée de même numéro.
    let mut par_sortie: Vec<Option<Vec<f32>>> = vec![None; n_out];
    let dossier = nom.rsplit_once('/').map_or("", |(d, _)| d);
    for c in &chemins {
        if c.entrees.len() != 1 || c.sorties.len() != 1 || c.entrees[0] != c.sorties[0] {
            return Err(RefusImport::new(
                "matrix_routing",
                format!("{nom} : le chemin de « {} » mélange des canaux", c.fichier),
            ));
        }
        let sortie = c.sorties[0];
        if sortie >= n_out {
            return Err(RefusImport::new(
                "invalid_cfg",
                format!("{nom} : canal {sortie} hors des {n_out} canaux"),
            ));
        }
        if par_sortie[sortie].is_some() {
            return Err(RefusImport::new(
                "matrix_routing",
                format!("{nom} : deux chemins vers la sortie {sortie}"),
            ));
        }
        let relatif = if dossier.is_empty() {
            c.fichier.clone()
        } else {
            format!("{dossier}/{}", c.fichier)
        };
        let Some(cle) = chemin_sur(&relatif) else {
            return Err(RefusImport::new(
                "unsafe_path",
                format!("{nom} : chemin hors de l'archive « {} »", c.fichier),
            ));
        };
        let cle = cle.to_ascii_lowercase();
        let octets = entrees.get(&cle).ok_or_else(|| {
            RefusImport::new(
                "invalid_cfg",
                format!("{nom} : fichier « {} » absent de l'archive", c.fichier),
            )
        })?;
        if !decodes.contains_key(&cle) {
            let (taps, sr_fichier) = lire_wav(octets, &c.fichier)?;
            consommer(budget, echantillons(&taps), nom)?;
            decodes.insert(cle.clone(), (taps, sr_fichier));
        }
        let (taps, sr_fichier) = &decodes[&cle];
        let sr_fichier = *sr_fichier;
        if sr_fichier != sr {
            return Err(RefusImport::new(
                "invalid_cfg",
                format!(
                    "{nom} : « {} » est à {sr_fichier} Hz, le .cfg annonce {sr} Hz",
                    c.fichier
                ),
            ));
        }
        let canal = taps.get(c.canal_fichier).ok_or_else(|| {
            RefusImport::new(
                "invalid_cfg",
                format!(
                    "{nom} : « {} » n'a pas de canal {}",
                    c.fichier, c.canal_fichier
                ),
            )
        })?;
        let ms = retard_entree[sortie] + retard_sortie[sortie];
        let decalage = (ms * f64::from(sr) / 1000.0).round() as usize;
        // Retard et longueur contrôlés AVANT l'allocation.
        let longueur = decalage.saturating_add(canal.len());
        if longueur > par_cadence::PRISES_MAX {
            return Err(RefusImport::new(
                "invalid_cfg",
                format!(
                    "{nom} : retard de {ms} ms et {} prises : au-delà de {} prises",
                    canal.len(),
                    par_cadence::PRISES_MAX
                ),
            ));
        }
        consommer(budget, longueur, nom)?;
        let mut r = Vec::with_capacity(longueur);
        r.resize(decalage, 0.0f32);
        r.extend_from_slice(canal);
        par_sortie[sortie] = Some(r);
        if !utilises.contains(&cle) {
            utilises.push(cle);
        }
    }
    let mut taps = Vec::with_capacity(n_out);
    for (i, r) in par_sortie.into_iter().enumerate() {
        taps.push(r.ok_or_else(|| {
            RefusImport::new(
                "invalid_cfg",
                format!("{nom} : aucun chemin vers la sortie {i}"),
            )
        })?);
    }
    let n = taps.iter().map(Vec::len).max().unwrap_or(0);
    let complement: usize = taps.iter().map(|t| n - t.len()).sum();
    consommer(budget, complement, nom)?;
    for t in &mut taps {
        t.resize(n, 0.0);
    }
    par_cadence::ri_admise(&taps, sr)
        .map_err(|e| RefusImport::new("invalid_cfg", format!("{nom} : {e}")))?;
    Ok(RiImportee {
        nom: nom.to_string(),
        sample_rate: sr,
        taps,
        origine: Origine::Cfg,
    })
}

fn importer_cfg(entrees: &BTreeMap<String, Vec<u8>>) -> Result<JeuImporte, RefusImport> {
    let mut jeu = JeuImporte::default();
    let mut utilises = Vec::new();
    let cfgs = entrees.keys().filter(|k| extension(k) == "cfg").count();
    if cfgs > REPONSES_MAX {
        return Err(RefusImport::new(
            "too_many_entries",
            format!("{cfgs} fichiers .cfg (au plus {REPONSES_MAX})"),
        ));
    }
    let mut decodes = Decodes::new();
    let mut budget = ECHANTILLONS_MAX;
    for (nom, octets) in entrees.iter().filter(|(k, _)| extension(k) == "cfg") {
        let texte = String::from_utf8_lossy(octets);
        let ri = lire_cfg(
            nom,
            &texte,
            entrees,
            &mut utilises,
            &mut decodes,
            &mut budget,
        )?;
        if let Some(autre) = jeu
            .reponses
            .iter()
            .find(|r| r.sample_rate == ri.sample_rate)
        {
            return Err(RefusImport::new(
                "duplicate_rate",
                format!(
                    "{} et {nom} sont tous deux à {} Hz",
                    autre.nom, ri.sample_rate
                ),
            ));
        }
        jeu.reponses.push(ri);
    }
    for nom in entrees.keys().filter(|k| extension(k) == "wav") {
        if !utilises.contains(nom) {
            jeu.ecartes
                .push((nom.clone(), "non référencé par un .cfg".to_string()));
        }
    }
    Ok(jeu)
}

#[cfg(test)]
#[path = "import_ri_tests.rs"]
mod tests;
