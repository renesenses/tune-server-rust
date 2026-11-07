//! C5 (#7919) : simuler les modes de basse d'une pièce rectangulaire, sans
//! micro, et en tirer une correction prudente par le moteur de C2.
//!
//! ```text
//! dimensions L × l × h, 6 surfaces (réfléchissante / amortie / traitée),
//! auditeur, enceintes
//!    │
//!    ├─ modes (nx, ny, nz) jusqu'à F_MAX_HZ : axiaux, tangentiels, obliques
//!    │     f = c/2 · √((nx/L)² + (ny/l)² + (nz/h)²)
//!    │     amortissement δ par les surfaces que le mode met en jeu → Q, T60
//!    │     couplage Σ ψ(enceinte)·ψ(auditeur), ψ = Π cos(nπx/L)
//!    ├─ réponse estimée à la place d'écoute (somme modale + son direct)
//!    └─ correction : `correction::proposer` (C2) avec des bornes PLUS
//!       serrées qu'une mesure ([`BANDES_MAX_ESTIMATION`],
//!       [`COUPE_MAX_DB_ESTIMATION`]), coupes seules, marquée estimation
//! ```
//!
//! Module pur : aucune E/S, aucun son. Le modèle idéalise la pièce (murs
//! rigides plans, absorption uniforme par surface, enceintes ponctuelles et
//! omnidirectionnelles dans le grave) : c'est une ESTIMATION, que tout ce
//! qui sort d'ici dit, et qu'une mesure au micro (C1) remplace.
//!
//! Choix du modèle (documentés aussi dans `docs/etudes/7632-calibration.md`) :
//!
//! * **absorption** par niveau de surface, dans le grave (≈ 63–125 Hz) :
//!   réfléchissante 0,05 (béton, plâtre sur maçonnerie, carrelage), amortie
//!   0,20 (cloison légère, parquet sur lambourdes, mobilier), traitée 0,50
//!   (pièges à basses, panneaux épais) ;
//! * **amortissement d'un mode** (approximation de Morse et Bolt, reprise par
//!   Kuttruff, *Room Acoustics*, ch. 3) : δ = c/(8V) · Σ ε·α·S sur les six
//!   surfaces, ε = 2 pour une surface perpendiculaire à un axe d'indice non
//!   nul (le mode la frappe de face), 1 sinon (il la rase) ; Q = π·f/δ,
//!   T60 = 3·ln 10/δ ;
//! * **réponse** : fonction de Green modale d'une source ponctuelle,
//!   Σ ψ(s)ψ(r) / (V·Λ·(k_n² − k² + 2j·δ·k/c)), Λ = Π (½ par indice non nul),
//!   sommée sur les modes jusqu'à [`F_CALCUL_HZ`] (au-dessus de la plage
//!   rendue, pour que le haut de la plage ne s'effondre pas), plus le son
//!   direct e^{−jkr}/(4πr) de chaque enceinte. Le mode (0, 0, 0) — la
//!   montée « en chambre de pression » sous le premier mode — n'est pas
//!   compté : la chute propre de l'enceinte, inconnue ici, la compense, et
//!   la compter ferait couper le bas du spectre sur une supposition. Les
//!   enceintes jouent le grave en phase (signal mono) ;
//! * **c = 343 m/s** (air à 20 °C).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tune_calibration::analysis::LogResponse;

use crate::correction::{DemandeCorrection, Proposition, proposer};

/// Vitesse du son (m/s, air à 20 °C).
pub const C_SON: f64 = 343.0;
/// Les modes rendus vont jusque-là : au-delà, la pièce ne se décrit plus
/// par ses modes (fréquence de Schroeder d'un salon : 150–300 Hz).
pub const F_MAX_HZ: f64 = 300.0;
/// La réponse somme les modes jusque-là, pour que la plage rendue ne
/// s'arrête pas net sur le dernier mode compté.
pub const F_CALCUL_HZ: f64 = 2.0 * F_MAX_HZ;
/// La réponse estimée couvre la zone de correction de C2 en un point.
pub const F_MIN_REPONSE_HZ: f64 = 20.0;
/// Points par octave de la réponse estimée.
pub const POINTS_PAR_OCTAVE: usize = 48;
/// Bornes des dimensions (m) : une pièce d'habitation ou un auditorium
/// modeste ; au-delà, le nombre de modes et le modèle n'ont plus de sens.
pub const COTE_MIN_M: f64 = 1.5;
pub const COTE_MAX_M: f64 = 20.0;
pub const HAUTEUR_MAX_M: f64 = 10.0;
pub const ENCEINTES_MAX: usize = 4;

/// Une estimation est plus prudente qu'une mesure : moins de bandes, moins
/// de coupe cumulée qu'en C2 (`correction::BANDES_MAX` = 10,
/// `correction::COUPE_MAX_DB` = 12). Une demande au-delà est REFUSÉE, pas
/// écrêtée, comme en C2.
pub const BANDES_MAX_ESTIMATION: usize = 4;
pub const COUPE_MAX_DB_ESTIMATION: f64 = 6.0;
/// Les défauts, sous les maxima.
pub const BANDES_DEFAUT_ESTIMATION: usize = 3;
pub const COUPE_DEFAUT_DB_ESTIMATION: f64 = 4.0;

/// Ce que dit toute sortie de ce module.
pub const AVERTISSEMENT_ESTIMATION: &str = "estimation sans micro : pièce rectangulaire, murs idéalisés et enceintes ponctuelles ; à confirmer par une mesure au micro";

/// La qualité d'une surface dans le grave, sur trois niveaux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Qualite {
    /// Béton, plâtre sur maçonnerie, carrelage, verre épais.
    Reflechissante,
    /// Cloison légère, parquet, mobilier, rideaux : le cas d'un salon.
    #[default]
    Amortie,
    /// Pièges à basses, panneaux épais.
    Traitee,
}

impl Qualite {
    /// Coefficient d'absorption dans le grave (≈ 63–125 Hz).
    pub fn absorption(self) -> f64 {
        match self {
            Qualite::Reflechissante => 0.05,
            Qualite::Amortie => 0.20,
            Qualite::Traitee => 0.50,
        }
    }
}

/// Les six surfaces. Repère : x de l'avant (mur derrière les enceintes,
/// x = 0) vers l'arrière, y de la gauche (y = 0) vers la droite, z du sol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Surfaces {
    #[serde(default)]
    pub avant: Qualite,
    #[serde(default)]
    pub arriere: Qualite,
    #[serde(default)]
    pub gauche: Qualite,
    #[serde(default)]
    pub droite: Qualite,
    #[serde(default)]
    pub sol: Qualite,
    #[serde(default)]
    pub plafond: Qualite,
}

impl Surfaces {
    pub fn toutes(q: Qualite) -> Self {
        Self {
            avant: q,
            arriere: q,
            gauche: q,
            droite: q,
            sol: q,
            plafond: q,
        }
    }
}

/// Dimensions intérieures (m) : longueur selon x, largeur selon y, hauteur.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct Piece {
    pub longueur_m: f64,
    pub largeur_m: f64,
    pub hauteur_m: f64,
}

impl Piece {
    pub fn volume(&self) -> f64 {
        self.longueur_m * self.largeur_m * self.hauteur_m
    }
    fn cotes(&self) -> [f64; 3] {
        [self.longueur_m, self.largeur_m, self.hauteur_m]
    }
}

/// Une position dans la pièce (m), dans le repère de [`Surfaces`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Position {
    fn coords(&self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
    fn distance(&self, autre: &Position) -> f64 {
        ((self.x - autre.x).powi(2) + (self.y - autre.y).powi(2) + (self.z - autre.z).powi(2))
            .sqrt()
    }
}

/// Ce que `POST /simulations` reçoit.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Entree {
    pub piece: Piece,
    #[serde(default)]
    pub surfaces: Surfaces,
    pub auditeur: Position,
    pub enceintes: Vec<Position>,
    /// La zone dont le profil de pièce sera enregistré ; facultative tant
    /// qu'on ne demande pas le profil.
    #[serde(default)]
    pub zone_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeMode {
    /// Un seul indice non nul : entre deux surfaces opposées.
    Axial,
    /// Deux indices non nuls : entre quatre surfaces.
    Tangentiel,
    /// Trois indices non nuls : entre les six.
    Oblique,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mode {
    pub frequence_hz: f64,
    #[serde(rename = "type")]
    pub type_mode: TypeMode,
    pub indices: [u32; 3],
    /// Facteur de qualité (f / largeur à −3 dB).
    pub q: f64,
    /// Durée de décroissance de 60 dB du mode seul.
    pub t60_s: f64,
    /// Σ ψ(enceinte)·ψ(auditeur) : 0 quand l'enceinte ou l'auditeur est sur
    /// un nœud du mode, ±1 par enceinte sur un ventre ; le signe dit si les
    /// enceintes s'ajoutent ou s'annulent sur ce mode.
    pub couplage: f64,
}

/// Ce que `POST /simulations` rend (avec l'identifiant posé par la route).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Simulation {
    pub entree: Entree,
    /// Toujours vrai : ce n'est pas une mesure.
    pub estimation: bool,
    pub volume_m3: f64,
    /// RT60 de Sabine de la pièce entière, avec les absorptions du grave.
    pub rt60_sabine_s: f64,
    /// Fréquence de Schroeder 2000·√(T/V) : au-dessus, les modes se
    /// recouvrent et le modèle perd son sens.
    pub f_schroeder_hz: f64,
    /// Les modes jusqu'à [`F_MAX_HZ`], par fréquence croissante.
    pub modes: Vec<Mode>,
    /// Réponse estimée à la place d'écoute, en dB relatifs à sa médiane.
    pub reponse: ReponseEstimee,
    pub avertissements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReponseEstimee {
    pub freqs_hz: Vec<f64>,
    pub db: Vec<f64>,
}

impl Simulation {
    pub fn reponse_log(&self) -> LogResponse {
        LogResponse {
            freqs: self.reponse.freqs_hz.clone(),
            magnitude_db: self.reponse.db.clone(),
        }
    }
}

/// Les contrôles d'entrée, avec un refus motivé.
pub fn valider(e: &Entree) -> Result<(), String> {
    let p = &e.piece;
    for (nom, v, max) in [
        ("longueur", p.longueur_m, COTE_MAX_M),
        ("largeur", p.largeur_m, COTE_MAX_M),
        ("hauteur", p.hauteur_m, HAUTEUR_MAX_M),
    ] {
        if !v.is_finite() || !(COTE_MIN_M..=max).contains(&v) {
            return Err(format!(
                "{nom} de la pièce {v} m : entre {COTE_MIN_M} et {max} m"
            ));
        }
    }
    if e.enceintes.is_empty() || e.enceintes.len() > ENCEINTES_MAX {
        return Err(format!(
            "{} enceinte(s) : entre 1 et {ENCEINTES_MAX}",
            e.enceintes.len()
        ));
    }
    let dedans = |nom: &str, q: &Position| -> Result<(), String> {
        for (axe, v, cote) in [
            ("x", q.x, p.longueur_m),
            ("y", q.y, p.largeur_m),
            ("z", q.z, p.hauteur_m),
        ] {
            if !v.is_finite() || !(0.0..=cote).contains(&v) {
                return Err(format!(
                    "{nom} : {axe} = {v} m hors de la pièce (0 à {cote} m)"
                ));
            }
        }
        Ok(())
    };
    dedans("auditeur", &e.auditeur)?;
    for (i, s) in e.enceintes.iter().enumerate() {
        dedans(&format!("enceinte {}", i + 1), s)?;
        if s.distance(&e.auditeur) < 0.1 {
            return Err(format!(
                "enceinte {} : à moins de 10 cm de l'auditeur",
                i + 1
            ));
        }
    }
    Ok(())
}

/// ψ_n(p) = Π cos(n_i·π·p_i / L_i).
pub fn forme_propre(indices: [u32; 3], piece: &Piece, p: &Position) -> f64 {
    let cotes = piece.cotes();
    let coords = p.coords();
    (0..3)
        .map(|i| (indices[i] as f64 * std::f64::consts::PI * coords[i] / cotes[i]).cos())
        .product()
}

pub fn frequence(indices: [u32; 3], piece: &Piece) -> f64 {
    let cotes = piece.cotes();
    let s: f64 = (0..3).map(|i| (indices[i] as f64 / cotes[i]).powi(2)).sum();
    C_SON / 2.0 * s.sqrt()
}

/// Constante d'amortissement δ (s⁻¹) du mode : la puissance absorbée par
/// chaque paire de surfaces, comptée double sur celles qu'il frappe de face.
pub fn amortissement(indices: [u32; 3], piece: &Piece, surfaces: &Surfaces) -> f64 {
    let [l, w, h] = piece.cotes();
    // Par axe : (surface d'une paroi, absorptions des deux parois).
    let paires = [
        (
            w * h,
            surfaces.avant.absorption() + surfaces.arriere.absorption(),
        ),
        (
            l * h,
            surfaces.gauche.absorption() + surfaces.droite.absorption(),
        ),
        (
            l * w,
            surfaces.sol.absorption() + surfaces.plafond.absorption(),
        ),
    ];
    let somme: f64 = (0..3)
        .map(|i| {
            let eps = if indices[i] == 0 { 1.0 } else { 2.0 };
            eps * paires[i].0 * paires[i].1
        })
        .sum();
    C_SON / (8.0 * piece.volume()) * somme
}

/// Tous les modes (hors (0, 0, 0)) jusqu'à `f_max`, par fréquence croissante.
fn enumerer(piece: &Piece, f_max: f64) -> Vec<[u32; 3]> {
    let cotes = piece.cotes();
    let n_max: Vec<u32> = cotes
        .iter()
        .map(|c| (2.0 * c * f_max / C_SON).floor() as u32)
        .collect();
    let mut v = Vec::new();
    for nx in 0..=n_max[0] {
        for ny in 0..=n_max[1] {
            for nz in 0..=n_max[2] {
                let n = [nx, ny, nz];
                if n == [0, 0, 0] {
                    continue;
                }
                if frequence(n, piece) <= f_max {
                    v.push(n);
                }
            }
        }
    }
    v.sort_by(|a, b| frequence(*a, piece).total_cmp(&frequence(*b, piece)));
    v
}

/// La simulation entière, après [`valider`].
pub fn simuler(entree: &Entree) -> Result<Simulation, String> {
    valider(entree)?;
    let piece = &entree.piece;
    let surf = &entree.surfaces;
    let v = piece.volume();
    let [l, w, h] = piece.cotes();
    let aire_absorption = w * h * (surf.avant.absorption() + surf.arriere.absorption())
        + l * h * (surf.gauche.absorption() + surf.droite.absorption())
        + l * w * (surf.sol.absorption() + surf.plafond.absorption());
    let rt60 = 0.161 * v / aire_absorption;
    let f_schroeder = 2000.0 * (rt60 / v).sqrt();

    let couplage = |n: [u32; 3]| -> f64 {
        let r = forme_propre(n, piece, &entree.auditeur);
        entree
            .enceintes
            .iter()
            .map(|s| forme_propre(n, piece, s) * r)
            .sum()
    };

    let tous = enumerer(piece, F_CALCUL_HZ);
    let modes: Vec<Mode> = tous
        .iter()
        .filter(|n| frequence(**n, piece) <= F_MAX_HZ)
        .map(|n| {
            let f = frequence(*n, piece);
            let delta = amortissement(*n, piece, surf);
            Mode {
                frequence_hz: f,
                type_mode: match n.iter().filter(|i| **i != 0).count() {
                    1 => TypeMode::Axial,
                    2 => TypeMode::Tangentiel,
                    _ => TypeMode::Oblique,
                },
                indices: *n,
                q: std::f64::consts::PI * f / delta,
                t60_s: 3.0 * std::f64::consts::LN_10 / delta,
                couplage: couplage(*n),
            }
        })
        .collect();

    // Réponse : somme modale complexe + son direct, à chaque fréquence.
    let termes: Vec<(f64, f64, f64)> = tous
        .iter()
        .map(|n| {
            let k_n = 2.0 * std::f64::consts::PI * frequence(*n, piece) / C_SON;
            let inv_lambda = 2f64.powi(n.iter().filter(|i| **i != 0).count() as i32);
            (
                k_n,
                amortissement(*n, piece, surf),
                couplage(*n) * inv_lambda / v,
            )
        })
        .collect();
    let distances: Vec<f64> = entree
        .enceintes
        .iter()
        .map(|s| s.distance(&entree.auditeur))
        .collect();
    // Un pas de grille au-delà de F_MAX_HZ : `grid` s'arrête au dernier point
    // sous sa borne, et le moteur de C2 exige une réponse qui couvre sa zone.
    let freqs = LogResponse::grid(
        F_MIN_REPONSE_HZ,
        F_MAX_HZ * 2f64.powf(1.0 / POINTS_PAR_OCTAVE as f64),
        POINTS_PAR_OCTAVE,
    );
    let mut db: Vec<f64> = freqs
        .iter()
        .map(|f| {
            let k = 2.0 * std::f64::consts::PI * f / C_SON;
            let (mut re, mut im) = (0.0, 0.0);
            for (k_n, delta, poids) in &termes {
                // poids / (k_n² − k² + j·2δk/c)
                let a = k_n * k_n - k * k;
                let b = 2.0 * delta * k / C_SON;
                let m = a * a + b * b;
                re += poids * a / m;
                im -= poids * b / m;
            }
            for r in &distances {
                let g = 1.0 / (4.0 * std::f64::consts::PI * r);
                re += g * (k * r).cos();
                im -= g * (k * r).sin();
            }
            10.0 * (re * re + im * im).max(1e-30).log10()
        })
        .collect();
    let mut tri = db.clone();
    tri.sort_by(f64::total_cmp);
    let mediane = tri[tri.len() / 2];
    for d in &mut db {
        *d -= mediane;
    }

    let mut avertissements = vec![AVERTISSEMENT_ESTIMATION.to_string()];
    if f_schroeder < F_MAX_HZ {
        avertissements.push(format!(
            "au-dessus d'environ {f_schroeder:.0} Hz (fréquence de Schroeder), les modes se recouvrent : l'estimation n'y décrit plus la pièce"
        ));
    }
    Ok(Simulation {
        entree: entree.clone(),
        estimation: true,
        volume_m3: v,
        rt60_sabine_s: rt60,
        f_schroeder_hz: f_schroeder,
        modes,
        reponse: ReponseEstimee {
            freqs_hz: freqs,
            db,
        },
        avertissements,
    })
}

/// La demande de correction ramenée aux bornes d'une estimation : défauts
/// plus bas, maxima plus bas, zone limitée à la plage simulée. Ce qui
/// dépasse est refusé, avec le motif.
pub fn demande_prudente(d: &DemandeCorrection) -> Result<DemandeCorrection, String> {
    let mut d = d.clone();
    let bandes = d.bandes_max.unwrap_or(BANDES_DEFAUT_ESTIMATION);
    if !(1..=BANDES_MAX_ESTIMATION).contains(&bandes) {
        return Err(format!(
            "{bandes} bandes : entre 1 et {BANDES_MAX_ESTIMATION} sur une estimation sans micro"
        ));
    }
    let coupe = d.coupe_max_db.unwrap_or(COUPE_DEFAUT_DB_ESTIMATION);
    if !coupe.is_finite() || coupe <= 0.0 || coupe > COUPE_MAX_DB_ESTIMATION {
        return Err(format!(
            "coupe maximale {coupe} dB : entre 0 et {COUPE_MAX_DB_ESTIMATION} dB sur une estimation sans micro"
        ));
    }
    if let Some(f) = d.f_max_hz
        && f > F_MAX_HZ
    {
        return Err(format!(
            "zone de correction jusqu'à {f} Hz : l'estimation s'arrête à {F_MAX_HZ} Hz"
        ));
    }
    d.bandes_max = Some(bandes);
    d.coupe_max_db = Some(coupe);
    Ok(d)
}

/// Les simulations gardées en mémoire, pour que la correction et le profil
/// se calculent sur celle qu'on a affichée. Rien n'est écrit sur disque : un
/// redémarrage les oublie, ce qui ne coûte qu'un nouveau calcul.
pub struct Magasin {
    entrees: Mutex<VecDeque<(String, Arc<Simulation>)>>,
    compteur: AtomicU64,
}

/// Au-delà, la plus ancienne est oubliée.
pub const SIMULATIONS_GARDEES: usize = 32;

impl Default for Magasin {
    fn default() -> Self {
        Self {
            entrees: Mutex::new(VecDeque::new()),
            compteur: AtomicU64::new(0),
        }
    }
}

impl Magasin {
    fn verrou(&self) -> std::sync::MutexGuard<'_, VecDeque<(String, Arc<Simulation>)>> {
        self.entrees.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Range la simulation et rend son identifiant.
    pub fn ranger(&self, sim: Simulation) -> String {
        let n = self.compteur.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("sim-{n}");
        let mut v = self.verrou();
        v.push_back((id.clone(), Arc::new(sim)));
        while v.len() > SIMULATIONS_GARDEES {
            v.pop_front();
        }
        id
    }

    pub fn simulation(&self, id: &str) -> Option<Arc<Simulation>> {
        self.verrou()
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, s)| s.clone())
    }
}

/// La proposition C2 sur la réponse estimée, bornée par [`demande_prudente`]
/// et marquée comme estimation.
pub fn proposer_estimation(
    sim: &Simulation,
    demande: &DemandeCorrection,
) -> Result<Proposition, String> {
    let d = demande_prudente(demande)?;
    let mut p = proposer(&sim.reponse_log(), &d)?;
    // Le C2 parle d'une « mesure en un point » : ici, rien n'a été mesuré.
    p.avertissements
        .retain(|a| !a.contains("position du micro"));
    p.avertissements
        .insert(0, AVERTISSEMENT_ESTIMATION.to_string());
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salon(surfaces: Surfaces) -> Entree {
        Entree {
            piece: Piece {
                longueur_m: 5.0,
                largeur_m: 4.0,
                hauteur_m: 2.5,
            },
            surfaces,
            auditeur: Position {
                x: 3.2,
                y: 2.0,
                z: 1.1,
            },
            enceintes: vec![
                Position {
                    x: 0.7,
                    y: 1.0,
                    z: 0.9,
                },
                Position {
                    x: 0.7,
                    y: 3.0,
                    z: 0.9,
                },
            ],
            zone_id: Some(7),
        }
    }

    /// 5 × 4 × 2,5 m : c/2L = 34,3 Hz, c/2l = 42,9 Hz, c/L = 68,6 Hz, puis
    /// c/2h = 68,6 Hz aussi (hauteur moitié de la longueur).
    #[test]
    fn les_premiers_modes_axiaux_d_une_piece_de_5_par_4_par_2_5() {
        let s = simuler(&salon(Surfaces::default())).unwrap();
        let axiaux: Vec<&Mode> = s
            .modes
            .iter()
            .filter(|m| m.type_mode == TypeMode::Axial)
            .collect();
        let attendus = [
            (34.3, [1, 0, 0]),
            (42.9, [0, 1, 0]),
            (68.6, [2, 0, 0]),
            (68.6, [0, 0, 1]),
        ];
        for (f, n) in attendus {
            let m = axiaux
                .iter()
                .find(|m| m.indices == n)
                .unwrap_or_else(|| panic!("mode {n:?} absent"));
            assert!(
                (m.frequence_hz - f).abs() < 0.1,
                "mode {n:?} : {:.2} Hz au lieu de {f} Hz",
                m.frequence_hz
            );
        }
        assert!(
            (s.modes[0].frequence_hz - 34.3).abs() < 0.1,
            "{:?}",
            s.modes[0]
        );
        assert_eq!(s.modes[0].indices, [1, 0, 0]);
        // Le premier tangentiel (1,1,0) : √(34,3² + 42,9²) = 54,9 Hz.
        let t = s.modes.iter().find(|m| m.indices == [1, 1, 0]).unwrap();
        assert_eq!(t.type_mode, TypeMode::Tangentiel);
        assert!((t.frequence_hz - 54.9).abs() < 0.1);
        assert_eq!(
            s.modes
                .iter()
                .find(|m| m.indices == [1, 1, 1])
                .unwrap()
                .type_mode,
            TypeMode::Oblique
        );
        assert!(s.modes.iter().all(|m| m.frequence_hz <= F_MAX_HZ));
        assert!(
            s.modes
                .windows(2)
                .all(|w| w[0].frequence_hz <= w[1].frequence_hz)
        );
        assert!(s.estimation);
    }

    /// Enceinte et auditeur au milieu de la longueur : nœud de (1,0,0), le
    /// mode n'y est pas excité ; contre un mur, ventre : couplage maximal.
    #[test]
    fn une_position_sur_un_noeud_ne_couple_pas_le_mode() {
        let mut e = salon(Surfaces::default());
        e.enceintes = vec![Position {
            x: 2.5,
            y: 0.5,
            z: 0.5,
        }];
        let s = simuler(&e).unwrap();
        let m = s.modes.iter().find(|m| m.indices == [1, 0, 0]).unwrap();
        assert!(
            m.couplage.abs() < 1e-9,
            "couplage {} sur un nœud",
            m.couplage
        );
        // Même chose côté auditeur.
        let mut e = salon(Surfaces::default());
        e.auditeur.y = 2.0; // milieu de la largeur : nœud de (0,1,0)
        let s = simuler(&e).unwrap();
        let m = s.modes.iter().find(|m| m.indices == [0, 1, 0]).unwrap();
        assert!(m.couplage.abs() < 1e-9, "couplage {}", m.couplage);
        // Enceinte et auditeur contre les murs avant/arrière : ventre.
        let mut e = salon(Surfaces::default());
        e.enceintes = vec![Position {
            x: 0.0,
            y: 0.5,
            z: 0.5,
        }];
        e.auditeur.x = 5.0;
        let s = simuler(&e).unwrap();
        let m = s.modes.iter().find(|m| m.indices == [1, 0, 0]).unwrap();
        assert!(
            m.couplage.abs() > 0.5,
            "couplage {} sur un ventre",
            m.couplage
        );
    }

    /// Un mode non couplé ne fait pas de pic dans la réponse ; couplé, si.
    #[test]
    fn la_reponse_suit_le_couplage() {
        let pic_a = |s: &Simulation, f: f64| {
            let i = s.reponse.freqs_hz.iter().position(|g| *g >= f).unwrap();
            s.reponse.db[i]
        };
        let mut e = salon(Surfaces::toutes(Qualite::Reflechissante));
        // Une seule enceinte et l'auditeur aux coins : tous les modes couplés.
        e.enceintes = vec![Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }];
        e.auditeur = Position {
            x: 5.0,
            y: 4.0,
            z: 2.5,
        };
        let coin = simuler(&e).unwrap();
        // Au milieu de la longueur : (1,0,0) éteint.
        e.enceintes[0].x = 2.5;
        let milieu = simuler(&e).unwrap();
        assert!(
            pic_a(&coin, 34.3) > pic_a(&milieu, 34.3) + 6.0,
            "34,3 Hz : {:.1} dB au coin, {:.1} dB au nœud",
            pic_a(&coin, 34.3),
            pic_a(&milieu, 34.3)
        );
        assert!(coin.reponse.db.iter().all(|d| d.is_finite()));
        assert_eq!(coin.reponse.freqs_hz.len(), coin.reponse.db.len());
        assert!(coin.reponse.freqs_hz[0] <= F_MIN_REPONSE_HZ);
        assert!(*coin.reponse.freqs_hz.last().unwrap() >= F_MAX_HZ);
    }

    /// Plus les surfaces absorbent, plus les modes sont amortis (Q plus bas).
    #[test]
    fn le_traitement_amortit_les_modes() {
        let q = |surf| {
            let s = simuler(&salon(surf)).unwrap();
            s.modes.iter().find(|m| m.indices == [1, 0, 0]).unwrap().q
        };
        let r = q(Surfaces::toutes(Qualite::Reflechissante));
        let a = q(Surfaces::toutes(Qualite::Amortie));
        let t = q(Surfaces::toutes(Qualite::Traitee));
        assert!(r > a && a > t, "Q : {r:.1} / {a:.1} / {t:.1}");
        // Traiter les seuls murs avant et arrière touche plus (1,0,0) que
        // (0,1,0), qu'ils ne font que raser.
        let surf = Surfaces {
            avant: Qualite::Traitee,
            arriere: Qualite::Traitee,
            ..Default::default()
        };
        let base = simuler(&salon(Surfaces::default())).unwrap();
        let traite = simuler(&salon(surf)).unwrap();
        let q_de = |s: &Simulation, n| s.modes.iter().find(|m| m.indices == n).unwrap().q;
        let baisse_x = q_de(&base, [1, 0, 0]) / q_de(&traite, [1, 0, 0]);
        let baisse_y = q_de(&base, [0, 1, 0]) / q_de(&traite, [0, 1, 0]);
        assert!(baisse_x > baisse_y, "{baisse_x:.2} contre {baisse_y:.2}");
    }

    /// La correction : coupes seules, jamais un creux remonté, et sous le
    /// plafond prudent — y compris quand la demande en réclame plus.
    #[test]
    fn la_correction_estimee_ne_fait_que_couper_sous_le_plafond_prudent() {
        let s = simuler(&salon(Surfaces::toutes(Qualite::Reflechissante))).unwrap();
        let p = proposer_estimation(&s, &DemandeCorrection::default()).unwrap();
        assert!(!p.bandes.is_empty(), "rien à couper sur une pièce nue");
        assert!(p.bandes.iter().all(|b| b.gain <= 0.0), "{:?}", p.bandes);
        assert!(p.gain_max_db <= 1e-9, "gain max {}", p.gain_max_db);
        assert!(
            p.bandes.len() <= BANDES_DEFAUT_ESTIMATION,
            "{} bandes par défaut sur une estimation",
            p.bandes.len()
        );
        assert!(
            p.coupe_cumulee_db >= -COUPE_DEFAUT_DB_ESTIMATION - 1e-6,
            "coupe cumulée {} dB par défaut sur une estimation",
            p.coupe_cumulee_db
        );
        for (m, c) in p.courbes.mesure_db.iter().zip(&p.courbes.corrigee_db) {
            assert!(c <= &(m + 1e-9), "un creux est remonté");
        }
        assert_eq!(p.avertissements[0], AVERTISSEMENT_ESTIMATION);

        // Aux maxima prudents.
        let d = DemandeCorrection {
            bandes_max: Some(BANDES_MAX_ESTIMATION),
            coupe_max_db: Some(COUPE_MAX_DB_ESTIMATION),
            ..Default::default()
        };
        let p = proposer_estimation(&s, &d).unwrap();
        assert!(p.bandes.len() <= BANDES_MAX_ESTIMATION);
        assert!(p.coupe_cumulee_db >= -COUPE_MAX_DB_ESTIMATION - 1e-6);

        // Au-delà : ce qu'une mesure permet est refusé sur une estimation.
        for (d, motif) in [
            (
                DemandeCorrection {
                    bandes_max: Some(8),
                    ..Default::default()
                },
                "entre 1 et 4",
            ),
            (
                DemandeCorrection {
                    coupe_max_db: Some(12.0),
                    ..Default::default()
                },
                "entre 0 et 6",
            ),
            (
                DemandeCorrection {
                    f_max_hz: Some(500.0),
                    ..Default::default()
                },
                "s'arrête à 300",
            ),
        ] {
            let e = proposer_estimation(&s, &d).expect_err("plafond prudent franchi");
            assert!(e.contains(motif), "{e}");
        }
    }

    #[test]
    fn une_entree_hors_de_la_piece_est_refusee() {
        let mut e = salon(Surfaces::default());
        e.auditeur.x = 6.0;
        assert!(simuler(&e).unwrap_err().contains("hors de la pièce"));
        let mut e = salon(Surfaces::default());
        e.piece.hauteur_m = f64::NAN;
        assert!(simuler(&e).unwrap_err().contains("hauteur"));
        let mut e = salon(Surfaces::default());
        e.enceintes.clear();
        assert!(simuler(&e).unwrap_err().contains("enceinte"));
    }
}
