//! Conception des filtres : nos propres sinc fenêtrés, à partir de la
//! littérature publique (fenêtrage de Kaiser, apodisation de Craven), sans
//! reprendre de conception propriétaire.
//!
//! Un filtre est décrit par quatre nombres : sa **portée** (le nombre
//! d'échantillons d'ENTRÉE que couvre sa réponse impulsionnelle), sa
//! **coupure** (fraction de la cadence d'entrée ; 0,5 = Nyquist), sa
//! **fenêtre** et sa **phase**. Le nombre de coefficients vaut `portée × L`
//! pour un facteur d'interpolation `L`, mais le coût par échantillon de
//! SORTIE ne dépend que de la portée : une phase du filtre polyphase n'a que
//! `portée` coefficients.
//!
//! Les rapports non entiers (44,1 → 384 ; 48 → 352,8…) se traitent en deux
//! étages : l'étage 1 est le filtre de la famille à un facteur entier
//! `⌊vers / de⌋`, l'étage 2 est un sinc court rationnel `L2/M2` dont la
//! transition est immense (de 22 kHz à plus de 300 kHz) et qui ne coûte
//! presque rien. Voir [`plan`] et [`etage_deux`].

use crate::fenetres;
use crate::phase_minimale::vers_phase_minimale;

/// Fenêtre appliquée au sinc.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fenetre {
    /// `alpha` = demi-longueur / écart-type (voir [`fenetres::gaussienne`]).
    Gaussienne { alpha: f64 },
    Kaiser { beta: f64 },
    BlackmanHarris,
}

impl Fenetre {
    pub fn echantillonner(self, n: usize) -> Vec<f64> {
        match self {
            Fenetre::Gaussienne { alpha } => fenetres::gaussienne(n, alpha),
            Fenetre::Kaiser { beta } => fenetres::kaiser(n, beta),
            Fenetre::BlackmanHarris => fenetres::blackman_harris(n),
        }
    }

    pub fn nom(self) -> String {
        match self {
            Fenetre::Gaussienne { alpha } => format!("gaussienne α={alpha}"),
            Fenetre::Kaiser { beta } => format!("Kaiser β={beta:.2}"),
            Fenetre::BlackmanHarris => "Blackman-Harris".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Lineaire,
    Minimale,
}

/// Une conception de filtre d'interpolation.
#[derive(Clone, Debug, PartialEq)]
pub struct Conception {
    pub id: &'static str,
    pub nom: &'static str,
    /// Nombre d'échantillons d'entrée couverts par la réponse impulsionnelle.
    pub portee: usize,
    /// Coupure à −6 dB, fraction de la cadence d'entrée (0,5 = Nyquist).
    pub coupure: f64,
    pub fenetre: Fenetre,
    pub phase: Phase,
    pub resume: &'static str,
}

/// Atténuation visée par les conceptions « précision » (Kaiser).
pub const ATTENUATION_PRECISION_DB: f64 = 150.0;

/// Les conceptions de l'étude. Les valeurs numériques sont justifiées dans
/// `docs/etudes/7614-surechantillonnage.md` ; les gabarits qui les
/// verrouillent sont dans `tests/gabarits.rs`.
pub fn catalogue() -> Vec<Conception> {
    let beta = fenetres::kaiser_beta(ATTENUATION_PRECISION_DB);
    // Coupure du « précision » : le bord de bande atténuée est posé EXACTEMENT
    // à Nyquist ; la coupure recule d'une demi-transition de Kaiser.
    let coupure_precision = |portee: usize| {
        0.5 - fenetres::kaiser_transition(portee, ATTENUATION_PRECISION_DB) / 2.0
    };
    let mut v = vec![
        Conception {
            id: "polyvalent",
            nom: "Polyvalent apodisant (gaussien)",
            portee: 256,
            coupure: 0.468,
            fenetre: Fenetre::Gaussienne { alpha: 5.0 },
            phase: Phase::Lineaire,
            resume: "Sinc à fenêtre gaussienne, portée 256 : réponse hors bande monotone (pas de lobes), −0,1 dB à 20 kHz et plus de 120 dB avant Nyquist à 44,1 kHz. Apodisant : il éteint ce que le filtre de l'ADC ou du mastering a laissé sonner à 22 kHz.",
        },
        Conception {
            id: "precision",
            nom: "Précision (sinc long, Kaiser 150 dB)",
            portee: 1024,
            coupure: coupure_precision(1024),
            fenetre: Fenetre::Kaiser { beta },
            phase: Phase::Lineaire,
            resume: "Sinc de 1 024 échantillons d'entrée fenêtré par Kaiser (β ≈ 15,6) : bande passante plate à 10⁻⁷ dB jusqu'à 21,6 kHz, 150 dB au-delà de Nyquist. La transition ne fait que 430 Hz à 44,1 kHz.",
        },
        Conception {
            id: "precision-longue",
            nom: "Précision longue (sinc 4 096, Kaiser 150 dB)",
            portee: 4096,
            coupure: coupure_precision(4096),
            fenetre: Fenetre::Kaiser { beta },
            phase: Phase::Lineaire,
            resume: "Même conception, portée quadruplée : transition de 107 Hz, bande plate jusqu'à 21,9 kHz. Quatre fois le coût du « précision » ; sert à mesurer la limite de la machine.",
        },
        Conception {
            id: "resolution",
            nom: "Résolution (transition étendue, réponse courte)",
            portee: 48,
            coupure: 0.5,
            fenetre: Fenetre::Gaussienne { alpha: 5.0 },
            phase: Phase::Lineaire,
            resume: "Sinc court (48 échantillons, 1,1 ms) à fenêtre gaussienne, −6 dB à Nyquist : la transition s'étale de 18,7 kHz à 29 kHz. Réponse impulsionnelle très courte, sans pré-écho visible ; laisse passer une partie des images entre 22 et 29 kHz, ce que le banc mesure.",
        },
    ];
    // Variantes en phase minimale : même module, toute l'énergie devant.
    let mut pm: Vec<Conception> = v
        .iter()
        .filter(|c| c.id != "precision-longue")
        .map(|c| {
            let (id, nom) = match c.id {
                "polyvalent" => ("polyvalent-pm", "Polyvalent apodisant, phase minimale"),
                "precision" => ("precision-pm", "Précision, phase minimale"),
                _ => ("resolution-pm", "Résolution, phase minimale"),
            };
            Conception {
                id,
                nom,
                phase: Phase::Minimale,
                resume: "Variante en phase minimale de la conception du même nom, obtenue par la méthode du cepstre : module identique, pas de pré-écho, latence réduite, phase non linéaire.",
                ..c.clone()
            }
        })
        .collect();
    v.append(&mut pm);
    v
}

pub fn par_id(id: &str) -> Option<Conception> {
    catalogue().into_iter().find(|c| c.id == id)
}

/// Sinc fenêtré de `n` coefficients, coupure `fc` en fraction de la cadence à
/// laquelle les coefficients sont pris (cycles par échantillon), normalisé à
/// une somme unité.
pub fn sinc_fenetre(n: usize, fc: f64, fenetre: Fenetre) -> Vec<f64> {
    assert!(n >= 2 && fc > 0.0 && fc <= 0.5);
    let w = fenetre.echantillonner(n);
    let c = (n - 1) as f64 / 2.0;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - c;
            let s = if x.abs() < 1e-12 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            s * w[i]
        })
        .collect();
    normaliser(&mut h, 1.0);
    h
}

/// Sommation compensée (Kahan) : la même précaution que le correctif #4079
/// de rubato — les queues du sinc sont minuscules devant la somme.
pub fn somme_compensee(v: &[f64]) -> f64 {
    let mut s = 0.0;
    let mut c = 0.0;
    for &x in v {
        let y = x - c;
        let t = s + y;
        c = (t - s) - y;
        s = t;
    }
    s
}

pub fn normaliser(h: &mut [f64], cible: f64) {
    let s = somme_compensee(h);
    assert!(s.abs() > 1e-12, "noyau de somme nulle");
    let g = cible / s;
    for v in h.iter_mut() {
        *v *= g;
    }
}

impl Conception {
    /// Le noyau complet pour un facteur d'interpolation `l` : `portee × l`
    /// coefficients pris à la cadence `l × Fs_in`, somme `l` (gain unité en
    /// continu après interpolation). En phase minimale, le noyau linéaire est
    /// transformé puis renormalisé.
    pub fn noyau(&self, l: usize) -> Vec<f64> {
        assert!(l >= 1);
        let n = self.portee * l;
        let mut h = sinc_fenetre(n, self.coupure / l as f64, self.fenetre);
        if self.phase == Phase::Minimale {
            h = vers_phase_minimale(&h);
        }
        normaliser(&mut h, l as f64);
        h
    }

    /// Délai de groupe théorique, en échantillons d'entrée (phase linéaire).
    pub fn latence_lineaire_trames(&self) -> f64 {
        (self.portee as f64 - 1.0) / 2.0
    }
}

/// Fraction réduite `l/m` de `vers / de`.
pub fn fraction(de: u32, vers: u32) -> (usize, usize) {
    fn pgcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { pgcd(b, a % b) }
    }
    let g = pgcd(de, vers);
    ((vers / g) as usize, (de / g) as usize)
}

/// Plan de conversion `de → vers`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub de: u32,
    pub vers: u32,
    /// Facteur entier de l'étage 1 (le filtre de la famille).
    pub l1: usize,
    /// Cadence après l'étage 1.
    pub intermediaire: u32,
    /// Rapport rationnel de l'étage 2 (`1/1` quand il n'y en a pas).
    pub l2: usize,
    pub m2: usize,
}

impl Plan {
    pub fn deux_etages(&self) -> bool {
        self.l2 != 1 || self.m2 != 1
    }
}

/// Décide des étages. Refuse la descente de cadence : le prototype ne fait
/// que suréchantillonner.
pub fn plan(de: u32, vers: u32) -> Result<Plan, String> {
    if de == 0 || vers < de {
        return Err(format!("le prototype ne fait que monter : {de} → {vers}"));
    }
    let l1 = (vers / de) as usize;
    let intermediaire = de * l1 as u32;
    let (l2, m2) = fraction(intermediaire, vers);
    Ok(Plan {
        de,
        vers,
        l1,
        intermediaire,
        l2,
        m2,
    })
}

/// Le sinc court de l'étage 2 pour un plan à deux étages, pris à la cadence
/// `intermediaire × l2`, somme `l2`.
///
/// Sa bande passante va jusqu'à `de/2` (tout ce qui reste du signal après
/// l'étage 1) ; sa bande atténuée commence à la première image,
/// `intermediaire − de/2`. La transition occupe donc presque toute la
/// cadence intermédiaire, et quelques dizaines de coefficients par phase
/// suffisent à 150 dB. Rend `(noyau, portee)`.
pub fn etage_deux(plan: &Plan) -> (Vec<f64>, usize) {
    assert!(plan.deux_etages());
    let fp = plan.de as f64 / 2.0;
    let fs = plan.intermediaire as f64 - fp;
    let transition = (fs - fp) / plan.intermediaire as f64;
    let portee = fenetres::kaiser_longueur(ATTENUATION_PRECISION_DB, transition).max(8);
    let coupure = (fp + fs) / 2.0 / plan.intermediaire as f64;
    let n = portee * plan.l2;
    let beta = fenetres::kaiser_beta(ATTENUATION_PRECISION_DB);
    let mut h = sinc_fenetre(n, coupure / plan.l2 as f64, Fenetre::Kaiser { beta });
    normaliser(&mut h, plan.l2 as f64);
    (h, portee)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_de_reference() {
        let p = plan(44_100, 352_800).unwrap();
        assert_eq!((p.l1, p.l2, p.m2), (8, 1, 1));
        let p = plan(44_100, 384_000).unwrap();
        assert_eq!((p.l1, p.intermediaire, p.l2, p.m2), (8, 352_800, 160, 147));
        let p = plan(48_000, 352_800).unwrap();
        assert_eq!((p.l1, p.intermediaire, p.l2, p.m2), (7, 336_000, 21, 20));
        let p = plan(44_100, 768_000).unwrap();
        assert_eq!((p.l1, p.l2, p.m2), (17, 2560, 2499));
        assert!(plan(96_000, 48_000).is_err());
    }

    #[test]
    fn noyau_normalise_par_phase() {
        for c in catalogue() {
            let l = 4;
            let h = c.noyau(l);
            assert_eq!(h.len(), c.portee * l);
            for p in 0..l {
                let s: f64 = h.iter().skip(p).step_by(l).sum();
                let tol = if c.phase == Phase::Lineaire { 1e-6 } else { 2e-3 };
                assert!((s - 1.0).abs() < tol, "{} phase {p} : somme {s}", c.id);
            }
        }
    }

    #[test]
    fn etage_deux_court() {
        let p = plan(44_100, 384_000).unwrap();
        let (h, portee) = etage_deux(&p);
        assert!(portee <= 24, "portée {portee}");
        assert_eq!(h.len(), portee * 160);
    }
}
