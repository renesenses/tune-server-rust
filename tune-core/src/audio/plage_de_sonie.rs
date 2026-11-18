//! #9040 — la PLAGE DE SONIE (EBU R128 *Loudness Range*, LRA) d'une piste et
//! d'un album (go de Bertrand du 18/11, forum fil 2902, suite de #9035).
//!
//! # Ce que mesure ce module
//!
//! La dispersion de la sonie à court terme d'un enregistrement, en LU, selon
//! EBU Tech 3342 :
//!
//! 1. la sonie à court terme (fenêtre de 3 s, pondération K de BS.1770) est
//!    prise toutes les 100 ms : 30 sous-blocs de 100 ms consécutifs. Tech 3342
//!    demande des fenêtres de 3 s qui se recouvrent d'au moins 2 s ; 2,9 s ici ;
//! 2. porte absolue : seules les fenêtres au-dessus de −70 LUFS comptent ;
//! 3. porte relative : seules celles à moins de 20 LU sous la sonie
//!    (moyenne des puissances) des fenêtres retenues au 2 ;
//! 4. `LRA = L95 − L10`, les centiles 10 et 95 des sonies restantes.
//!
//! La LRA n'est PAS le DR : le DR (TT, `analyzer::DrAccumulator`) mesure le
//! facteur de crête dans les passages forts — ce qu'un limiteur écrase ; la
//! LRA mesure l'écart entre passages calmes et forts — ce qu'un compresseur
//! lent resserre. Un master peut être limité (DR bas) et garder une grande
//! LRA, ou l'inverse. Les deux sont affichés, aucun n'est un verdict.
//!
//! # Coût
//!
//! Aucun décodage de plus : les sous-blocs de 100 ms sont les tranches que
//! l'accumulateur de sonie intégrée (`analyzer::LoudnessAccumulator`) retire
//! déjà de sa file, sur les échantillons déjà pondérés K. Une somme de carrés
//! de plus par échantillon et par canal, un `f64` par 100 ms en mémoire
//! (36 000 pour une heure), un tri à la fin.
//!
//! # L'album
//!
//! Tech 3342 calcule la LRA d'un programme sur TOUTES ses fenêtres : celle
//! d'un album n'est ni la moyenne ni le maximum des LRA de ses pistes (deux
//! pistes de LRA 0 à −10 et −20 LUFS font un album de LRA 10). Chaque piste
//! garde donc l'histogramme de ses sonies à court terme passées la porte
//! absolue, par cases de 0,1 LU (la méthode de `libebur128` pour plusieurs
//! programmes) ; l'album fusionne les histogrammes de ses pistes et applique
//! la porte relative et les centiles à l'ensemble. Écart avec le calcul
//! exact : au plus une case, 0,1 LU.

use std::collections::BTreeMap;

/// La version de l'algorithme, écrite avec chaque mesure.
pub const ALGO: &str = "ebu-tech3342-st100ms-v1";

/// Sous-blocs de 100 ms par fenêtre à court terme (3 s).
pub const SOUS_BLOCS_PAR_FENETRE: usize = 30;

/// Porte absolue, en LUFS.
const PORTE_ABSOLUE_LUFS: f64 = -70.0;
/// Porte relative, en LU sous la sonie des fenêtres retenues.
const PORTE_RELATIVE_LU: f64 = -20.0;
const CENTILE_BAS: f64 = 0.10;
const CENTILE_HAUT: f64 = 0.95;
/// Cases de l'histogramme par LU.
const CASES_PAR_LU: f64 = 10.0;

/// Puissance (moyenne des carrés pondérés K, canaux sommés) → sonie en LUFS.
fn sonie(puissance: f64) -> f64 {
    -0.691 + 10.0 * puissance.log10()
}

/// Sonie en LUFS → puissance.
fn puissance(lufs: f64) -> f64 {
    10f64.powf((lufs + 0.691) / 10.0)
}

fn arrondi_au_dixieme(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Les sonies à court terme (LUFS) des fenêtres de 3 s glissant de 100 ms,
/// passées la porte absolue. Vide si la piste a moins de 3 s.
pub fn courts_termes(sous_blocs: &[f64]) -> Vec<f64> {
    if sous_blocs.len() < SOUS_BLOCS_PAR_FENETRE {
        return Vec::new();
    }
    sous_blocs
        .windows(SOUS_BLOCS_PAR_FENETRE)
        .map(|w| w.iter().sum::<f64>() / SOUS_BLOCS_PAR_FENETRE as f64)
        .filter(|p| *p > 0.0)
        .map(sonie)
        .filter(|l| *l > PORTE_ABSOLUE_LUFS)
        .collect()
}

/// L'indice du centile `p` dans `n` valeurs triées — celui de `libebur128` :
/// `round((n − 1) · p)`.
fn indice_du_centile(n: usize, p: f64) -> usize {
    (((n - 1) as f64) * p).round() as usize
}

/// La LRA, en LU arrondie au dixième, de sonies à court terme déjà passées
/// la porte absolue. `None` sans aucune fenêtre.
pub fn lra(courts_termes: &[f64]) -> Option<f64> {
    if courts_termes.is_empty() {
        return None;
    }
    let moyenne =
        courts_termes.iter().map(|l| puissance(*l)).sum::<f64>() / courts_termes.len() as f64;
    let seuil = sonie(moyenne) + PORTE_RELATIVE_LU;
    let mut retenues: Vec<f64> = courts_termes
        .iter()
        .copied()
        .filter(|l| *l >= seuil)
        .collect();
    if retenues.is_empty() {
        return None;
    }
    retenues.sort_by(|a, b| a.total_cmp(b));
    let n = retenues.len();
    let bas = retenues[indice_du_centile(n, CENTILE_BAS)];
    let haut = retenues[indice_du_centile(n, CENTILE_HAUT)];
    Some(arrondi_au_dixieme((haut - bas).max(0.0)))
}

/// L'histogramme des sonies à court terme d'une piste, passées la porte
/// absolue : case `round(LUFS × 10)` → nombre de fenêtres.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Histogramme(BTreeMap<i32, u32>);

impl Histogramme {
    pub fn de(courts_termes: &[f64]) -> Self {
        let mut h = BTreeMap::new();
        for l in courts_termes {
            *h.entry((l * CASES_PAR_LU).round() as i32).or_insert(0u32) += 1;
        }
        Self(h)
    }

    pub fn fenetres(&self) -> u64 {
        self.0.values().map(|n| u64::from(*n)).sum()
    }

    /// Ajoute les fenêtres d'une autre piste (album).
    pub fn fusionner(&mut self, autre: &Histogramme) {
        for (case, n) in &autre.0 {
            *self.0.entry(*case).or_insert(0) += *n;
        }
    }

    /// La LRA de l'ensemble des fenêtres, chaque fenêtre prise au centre de
    /// sa case. `None` sans fenêtre.
    pub fn lra(&self) -> Option<f64> {
        let total = self.fenetres();
        if total == 0 {
            return None;
        }
        let somme: f64 = self
            .0
            .iter()
            .map(|(c, n)| puissance(f64::from(*c) / CASES_PAR_LU) * f64::from(*n))
            .sum();
        let seuil = sonie(somme / total as f64) + PORTE_RELATIVE_LU;
        let retenues: Vec<(f64, u64)> = self
            .0
            .iter()
            .map(|(c, n)| (f64::from(*c) / CASES_PAR_LU, u64::from(*n)))
            .filter(|(l, _)| *l >= seuil)
            .collect();
        let n: u64 = retenues.iter().map(|(_, k)| *k).sum();
        if n == 0 {
            return None;
        }
        let au_rang = |rang: u64| {
            let mut cumul = 0u64;
            for (l, k) in &retenues {
                cumul += k;
                if rang < cumul {
                    return *l;
                }
            }
            retenues.last().map(|(l, _)| *l).unwrap_or_default()
        };
        let bas = au_rang(indice_du_centile(n as usize, CENTILE_BAS) as u64);
        let haut = au_rang(indice_du_centile(n as usize, CENTILE_HAUT) as u64);
        Some(arrondi_au_dixieme((haut - bas).max(0.0)))
    }

    /// Forme stockée : `case:nombre` séparés par `;`, cases croissantes.
    pub fn encoder(&self) -> String {
        self.0
            .iter()
            .map(|(c, n)| format!("{c}:{n}"))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Relit [`Self::encoder`]. `None` sur une chaîne mal formée : une valeur
    /// douteuse ne doit pas entrer dans la LRA d'un album.
    pub fn decoder(brut: &str) -> Option<Self> {
        let mut h = BTreeMap::new();
        for paire in brut.split(';').filter(|p| !p.trim().is_empty()) {
            let (c, n) = paire.split_once(':')?;
            let c: i32 = c.trim().parse().ok()?;
            let n: u32 = n.trim().parse().ok()?;
            *h.entry(c).or_insert(0) += n;
        }
        Some(Self(h))
    }
}

/// La plage de sonie d'une piste : sa LRA et l'histogramme qui servira à
/// celle de son album.
#[derive(Debug, Clone, PartialEq)]
pub struct PlageDeSonie {
    pub lra_lu: f64,
    pub histogramme: Histogramme,
}

impl PlageDeSonie {
    /// Des sous-blocs de 100 ms (puissances pondérées K, canaux sommés).
    /// `None` pour une piste de moins de 3 s ou muette.
    pub fn des_sous_blocs(sous_blocs: &[f64]) -> Option<Self> {
        let st = courts_termes(sous_blocs);
        let lra_lu = lra(&st)?;
        Some(Self {
            lra_lu,
            histogramme: Histogramme::de(&st),
        })
    }
}

/// Écrit (ou remplace) la plage de sonie d'une piste. Même texte SQL pour
/// SQLite et PostgreSQL (`ON CONFLICT … DO UPDATE`, `?` traduits).
pub fn ecrire(
    tx: &dyn crate::db::backend::DbTxHandle,
    track_id: i64,
    p: &PlageDeSonie,
) -> Result<(), String> {
    use crate::db::backend::ToSqlValue;
    let histogramme = p.histogramme.encoder();
    let maintenant = crate::db::spectrogrammes::maintenant();
    tx.execute(
        "INSERT INTO plages_de_sonie (track_id, lra_lu, histogramme, algo, mesure_le) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (track_id) DO UPDATE SET lra_lu = excluded.lra_lu, \
           histogramme = excluded.histogramme, algo = excluded.algo, \
           mesure_le = excluded.mesure_le",
        &[
            &track_id as &dyn ToSqlValue,
            &p.lra_lu as &dyn ToSqlValue,
            &histogramme.as_str() as &dyn ToSqlValue,
            &ALGO as &dyn ToSqlValue,
            &maintenant.as_str() as &dyn ToSqlValue,
        ],
    )
    .map(|_| ())
}

#[cfg(test)]
#[path = "plage_de_sonie_tests_9040.rs"]
mod tests;
