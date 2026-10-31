//! Interpolateur polyphase à un ou deux étages, et son produit scalaire SIMD.
//!
//! Structure : pour un facteur `L` et un noyau de `portée × L` coefficients,
//! la phase `p` ne contient que les coefficients `h[p + L·q]`, `q = 0..portée`.
//! Chaque échantillon de sortie coûte donc `portée` multiplications-
//! accumulations, quel que soit `L` (Crochiere & Rabiner, *Multirate Digital
//! Signal Processing*, 1983, chap. 3). Le rapport rationnel `L/M` est la
//! même structure avec un pas de `M` positions sur la grille interpolée.
//!
//! L'historique d'entrée est tenu en double (chaque échantillon écrit deux
//! fois, à `w` et `w + portée`) : la fenêtre des `portée` dernières entrées
//! est toujours contiguë, le produit scalaire ne gère aucun rebouclage.

use std::sync::OnceLock;

use crate::conception::{self, Conception, Plan};

/// Chemin de calcul du produit scalaire, décidé une fois par processus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chemin {
    /// x86-64 avec AVX2 et FMA : quatre accumulateurs de quatre f64.
    Avx2Fma,
    /// aarch64 : la boucle à huit accumulateurs, vectorisée NEON par le
    /// compilateur (NEON fait partie de la base aarch64).
    Neon,
    Scalaire,
}

impl Chemin {
    pub fn nom(self) -> &'static str {
        match self {
            Chemin::Avx2Fma => "avx2+fma (f64×4, 4 accumulateurs)",
            Chemin::Neon => "neon (auto-vectorisé, 8 accumulateurs)",
            Chemin::Scalaire => "scalaire (8 accumulateurs)",
        }
    }
}

pub fn chemin_simd() -> Chemin {
    static CHEMIN: OnceLock<Chemin> = OnceLock::new();
    *CHEMIN.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
            {
                return Chemin::Avx2Fma;
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            return Chemin::Neon;
        }
        #[allow(unreachable_code)]
        Chemin::Scalaire
    })
}

/// Produit scalaire de deux tranches de même longueur.
#[inline]
pub fn produit_scalaire(a: &[f64], b: &[f64]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    match chemin_simd() {
        #[cfg(target_arch = "x86_64")]
        Chemin::Avx2Fma => unsafe { ps_avx2(a, b) },
        _ => ps_scalaire(a, b),
    }
}

/// Huit accumulateurs indépendants : le compilateur peut les vectoriser, et
/// la latence de l'additionneur n'enchaîne plus toute la somme (la leçon de
/// `DsdToPcmStreamer`, #4354).
pub fn ps_scalaire(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let mut acc = [0.0f64; 8];
    let blocs = n / 8;
    for i in 0..blocs {
        let x = &a[i * 8..i * 8 + 8];
        let y = &b[i * 8..i * 8 + 8];
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    let mut s = ((acc[0] + acc[1]) + (acc[2] + acc[3])) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    for i in blocs * 8..n {
        s += a[i] * b[i];
    }
    s
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn ps_avx2(a: &[f64], b: &[f64]) -> f64 {
    use std::arch::x86_64::*;
    let n = a.len().min(b.len());
    let mut i = 0usize;
    let mut s;
    unsafe {
        let mut acc0 = _mm256_setzero_pd();
        let mut acc1 = _mm256_setzero_pd();
        let mut acc2 = _mm256_setzero_pd();
        let mut acc3 = _mm256_setzero_pd();
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        while i + 16 <= n {
            acc0 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i)), _mm256_loadu_pd(pb.add(i)), acc0);
            acc1 = _mm256_fmadd_pd(
                _mm256_loadu_pd(pa.add(i + 4)),
                _mm256_loadu_pd(pb.add(i + 4)),
                acc1,
            );
            acc2 = _mm256_fmadd_pd(
                _mm256_loadu_pd(pa.add(i + 8)),
                _mm256_loadu_pd(pb.add(i + 8)),
                acc2,
            );
            acc3 = _mm256_fmadd_pd(
                _mm256_loadu_pd(pa.add(i + 12)),
                _mm256_loadu_pd(pb.add(i + 12)),
                acc3,
            );
            i += 16;
        }
        while i + 4 <= n {
            acc0 = _mm256_fmadd_pd(_mm256_loadu_pd(pa.add(i)), _mm256_loadu_pd(pb.add(i)), acc0);
            i += 4;
        }
        let acc = _mm256_add_pd(_mm256_add_pd(acc0, acc1), _mm256_add_pd(acc2, acc3));
        let mut t = [0.0f64; 4];
        _mm256_storeu_pd(t.as_mut_ptr(), acc);
        s = (t[0] + t[1]) + (t[2] + t[3]);
    }
    while i < n {
        s += a[i] * b[i];
        i += 1;
    }
    s
}

/// Un étage polyphase `L/M`, multicanal entrelacé.
pub struct Etage {
    l: usize,
    m: usize,
    portee: usize,
    canaux: usize,
    /// `l × portée`, chaque phase retournée (plus ancien d'abord).
    phases_inv: Vec<f64>,
    /// `canaux × 2 × portée`.
    hist: Vec<f64>,
    w: usize,
    prochaine_pos: u64,
    entrees: u64,
}

impl Etage {
    pub fn nouveau(noyau: &[f64], l: usize, m: usize, canaux: usize) -> Self {
        assert!(l >= 1 && m >= 1 && canaux >= 1);
        assert_eq!(noyau.len() % l, 0, "noyau non multiple de L");
        let portee = noyau.len() / l;
        let mut phases_inv = vec![0.0; l * portee];
        for p in 0..l {
            for j in 0..portee {
                phases_inv[p * portee + j] = noyau[p + l * (portee - 1 - j)];
            }
        }
        Self {
            l,
            m,
            portee,
            canaux,
            phases_inv,
            hist: vec![0.0; canaux * 2 * portee],
            w: 0,
            prochaine_pos: 0,
            entrees: 0,
        }
    }

    pub fn portee(&self) -> usize {
        self.portee
    }

    pub fn memoire_noyau_octets(&self) -> usize {
        self.phases_inv.len() * std::mem::size_of::<f64>()
    }

    /// Consomme des trames entrelacées, pousse les trames de sortie.
    pub fn traiter(&mut self, entree: &[f64], sortie: &mut Vec<f64>) {
        let c = self.canaux;
        let p = self.portee;
        let l = self.l as u64;
        for trame in entree.chunks_exact(c) {
            for (ch, &x) in trame.iter().enumerate() {
                let base = ch * 2 * p;
                self.hist[base + self.w] = x;
                self.hist[base + self.w + p] = x;
            }
            self.w += 1;
            if self.w == p {
                self.w = 0;
            }
            let i = self.entrees;
            self.entrees += 1;
            while self.prochaine_pos / l <= i {
                let phase = (self.prochaine_pos % l) as usize;
                let coefs = &self.phases_inv[phase * p..phase * p + p];
                for ch in 0..c {
                    let base = ch * 2 * p;
                    let fen = &self.hist[base + self.w..base + self.w + p];
                    sortie.push(produit_scalaire(coefs, fen));
                }
                self.prochaine_pos += self.m as u64;
            }
        }
    }
}

/// Le suréchantillonneur complet d'une conception pour un couple de cadences.
pub struct Surechantillonneur {
    conception: Conception,
    plan: Plan,
    canaux: usize,
    noyau1: Vec<f64>,
    etage1: Etage,
    etage2: Option<Etage>,
    inter: Vec<f64>,
}

impl Surechantillonneur {
    pub fn nouveau(
        conception: &Conception,
        de: u32,
        vers: u32,
        canaux: usize,
    ) -> Result<Self, String> {
        if canaux == 0 {
            return Err("aucun canal".into());
        }
        let plan = conception::plan(de, vers)?;
        let noyau1 = conception.noyau(plan.l1);
        let etage1 = Etage::nouveau(&noyau1, plan.l1, 1, canaux);
        let etage2 = if plan.deux_etages() {
            let (h, _) = conception::etage_deux(&plan);
            Some(Etage::nouveau(&h, plan.l2, plan.m2, canaux))
        } else {
            None
        };
        Ok(Self {
            conception: conception.clone(),
            plan,
            canaux,
            noyau1,
            etage1,
            etage2,
            inter: Vec::new(),
        })
    }

    pub fn conception(&self) -> &Conception {
        &self.conception
    }
    pub fn plan(&self) -> Plan {
        self.plan
    }
    pub fn canaux(&self) -> usize {
        self.canaux
    }
    /// Le noyau de l'étage 1 (pour les mesures de réponse).
    pub fn noyau_etage1(&self) -> &[f64] {
        &self.noyau1
    }
    pub fn portee_etage2(&self) -> usize {
        self.etage2.as_ref().map_or(0, Etage::portee)
    }
    pub fn memoire_noyaux_octets(&self) -> usize {
        self.etage1.memoire_noyau_octets()
            + self.etage2.as_ref().map_or(0, Etage::memoire_noyau_octets)
    }

    /// Multiplications-accumulations par trame de SORTIE et par canal.
    pub fn mac_par_trame_de_sortie(&self) -> f64 {
        let e1 = self.conception.portee as f64 * self.plan.intermediaire as f64
            / self.plan.vers as f64;
        e1 + self.portee_etage2() as f64
    }

    /// Délai de groupe théorique (phase linéaire), en trames d'entrée.
    pub fn latence_theorique_trames_entree(&self) -> f64 {
        let e1 = self.conception.latence_lineaire_trames();
        let e2 = if self.plan.deux_etages() {
            (self.portee_etage2() as f64 - 1.0) / 2.0 / self.plan.l1 as f64
        } else {
            0.0
        };
        e1 + e2
    }

    pub fn latence_theorique_ms(&self) -> f64 {
        self.latence_theorique_trames_entree() * 1000.0 / self.plan.de as f64
    }

    pub fn traiter(&mut self, entree: &[f64], sortie: &mut Vec<f64>) {
        match &mut self.etage2 {
            None => self.etage1.traiter(entree, sortie),
            Some(e2) => {
                self.inter.clear();
                self.etage1.traiter(entree, &mut self.inter);
                e2.traiter(&self.inter, sortie);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conception::par_id;

    #[test]
    fn produits_scalaires_identiques() {
        let a: Vec<f64> = (0..1037).map(|i| (i as f64 * 0.37).sin()).collect();
        let b: Vec<f64> = (0..1037).map(|i| (i as f64 * 0.11).cos()).collect();
        let r1 = ps_scalaire(&a, &b);
        let r2 = produit_scalaire(&a, &b);
        let r3: f64 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((r1 - r3).abs() < 1e-9 && (r2 - r3).abs() < 1e-9, "{r1} {r2} {r3}");
    }

    #[test]
    fn interpolation_entiere_compte_les_trames() {
        let c = par_id("resolution").unwrap();
        let mut s = Surechantillonneur::nouveau(&c, 48_000, 384_000, 2).unwrap();
        let entree = vec![0.0; 2 * 1000];
        let mut sortie = Vec::new();
        s.traiter(&entree, &mut sortie);
        assert_eq!(sortie.len(), 2 * 8000);
    }

    #[test]
    fn interpolation_rationnelle_compte_les_trames() {
        let c = par_id("resolution").unwrap();
        let mut s = Surechantillonneur::nouveau(&c, 44_100, 384_000, 1).unwrap();
        let entree = vec![0.0; 44_100];
        let mut sortie = Vec::new();
        s.traiter(&entree, &mut sortie);
        // 1 s d'entrée → 384 000 trames, au délai de l'étage 2 près.
        assert!((sortie.len() as i64 - 384_000).abs() <= 64, "{}", sortie.len());
    }

    #[test]
    fn impulsion_rend_le_noyau() {
        let c = par_id("resolution").unwrap();
        let mut s = Surechantillonneur::nouveau(&c, 44_100, 176_400, 1).unwrap();
        let mut entree = vec![0.0; c.portee + 2];
        entree[0] = 1.0;
        let mut sortie = Vec::new();
        s.traiter(&entree, &mut sortie);
        let h = s.noyau_etage1();
        // La sortie est le noyau complet, coefficient par coefficient : la
        // phase `p` de la trame `i` rend `h[p + L·i]`.
        for (k, &v) in h.iter().enumerate() {
            assert!((sortie[k] - v).abs() < 1e-15, "coef {k} : {} vs {v}", sortie[k]);
        }
    }
}
