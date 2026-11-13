//! La FORME D'ONDE d'une piste pour sa fiche technique (fil du forum 2793,
//! décision de Bertrand du 13/11/2026, rc12).
//!
//! Le but est de VOIR la tête du signal : crêtes, écrêtage, état de la
//! compression, comme un éditeur du type iZotope RX. Par canal et par colonne
//! de temps : le minimum et le maximum des échantillons (crêtes) et la valeur
//! efficace (RMS). Par canal et pour l'ensemble : la crête maximale, la RMS
//! globale, le facteur de crête (PLR, crête − RMS en dB) et le nombre
//! d'échantillons écrêtés (à [`SEUIL_ECRETAGE_DBFS`] ou au-dessus).
//!
//! Calculé en Rust natif sur le PCM que Tune décode lui-même (jamais ffmpeg),
//! comme le spectrogramme (#7857) : la piste est décodée par fenêtres de
//! [`FENETRE_S`] secondes et la mémoire reste bornée quelle que soit la
//! durée — chaque fenêtre est réduite en cases, et au-delà de
//! `4 × colonnes` cases deux voisines fusionnent. Aucun son n'est produit.
//!
//! Les niveaux sont normalisés à ±1 (pleine échelle = 0 dBFS). La RMS est la
//! valeur efficace brute : un sinus à −6 dBFS de crête a une RMS de −9 dBFS.

/// Durée d'une fenêtre de décodage, en secondes.
pub const FENETRE_S: f64 = 10.0;
pub const COLONNES_MIN: usize = 50;
pub const COLONNES_MAX: usize = 4000;
pub const COLONNES_DEFAUT: usize = 1000;
/// Un échantillon à −0,01 dBFS ou au-dessus compte comme écrêté : la pleine
/// échelle d'un fichier 16 bits (32767) est à −0,0003 dBFS.
pub const SEUIL_ECRETAGE_DBFS: f64 = -0.01;
/// En deçà, un niveau est du silence : pas de valeur en dBFS.
pub const PLANCHER_DBFS: f64 = -150.0;

/// Les colonnes demandées, ramenées dans les bornes.
pub fn borner_colonnes(colonnes: usize) -> usize {
    colonnes.clamp(COLONNES_MIN, COLONNES_MAX)
}

/// Une amplitude linéaire (0..1) en dBFS ; `None` pour le silence.
pub fn dbfs(amplitude: f64) -> Option<f64> {
    let db = 20.0 * amplitude.abs().log10();
    (db.is_finite() && db > PLANCHER_DBFS).then_some(db)
}

fn seuil_ecretage() -> f32 {
    10f64.powf(SEUIL_ECRETAGE_DBFS / 20.0) as f32
}

#[derive(Debug, Clone, Copy)]
struct Case {
    min: f32,
    max: f32,
    somme_carres: f64,
    n: u64,
}

impl Case {
    const VIDE: Case = Case {
        min: 0.0,
        max: 0.0,
        somme_carres: 0.0,
        n: 0,
    };

    fn ajouter(&mut self, x: f32) {
        if self.n == 0 {
            self.min = x;
            self.max = x;
        } else {
            self.min = self.min.min(x);
            self.max = self.max.max(x);
        }
        self.somme_carres += f64::from(x) * f64::from(x);
        self.n += 1;
    }

    fn fusionner(&mut self, autre: &Case) {
        if autre.n == 0 {
            return;
        }
        if self.n == 0 {
            *self = *autre;
            return;
        }
        self.min = self.min.min(autre.min);
        self.max = self.max.max(autre.max);
        self.somme_carres += autre.somme_carres;
        self.n += autre.n;
    }

    fn rms(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            (self.somme_carres / self.n as f64).sqrt()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Totaux {
    crete: f32,
    somme_carres: f64,
    n: u64,
    ecretes: u64,
}

/// L'accumulateur : on lui pousse des trames entrelacées normalisées à ±1.
pub struct Accumulateur {
    canaux: usize,
    sample_rate: u32,
    colonnes: usize,
    /// Trames par case.
    taille_case: u64,
    /// Les cases pleines, par canal.
    cases: Vec<Vec<Case>>,
    /// La case en cours, par canal, et ses trames.
    courante: Vec<Case>,
    trames_courante: u64,
    totaux: Vec<Totaux>,
    trames: u64,
    seuil: f32,
}

impl Accumulateur {
    /// `trames_estimees` : la durée connue de la piste (ou de la plage), en
    /// trames, pour dimensionner les cases ; sans elle, les cases grossissent
    /// au fil du décodage.
    pub fn new(
        sample_rate: u32,
        canaux: u32,
        colonnes: usize,
        trames_estimees: Option<u64>,
    ) -> Self {
        let canaux = canaux.max(1) as usize;
        let colonnes = colonnes.max(1);
        let taille_case = trames_estimees
            .map(|t| t / (2 * colonnes as u64))
            .unwrap_or(256)
            .max(1);
        Self {
            canaux,
            sample_rate: sample_rate.max(1),
            colonnes,
            taille_case,
            cases: vec![Vec::with_capacity(4 * colonnes); canaux],
            courante: vec![Case::VIDE; canaux],
            trames_courante: 0,
            totaux: vec![Totaux::default(); canaux],
            trames: 0,
            seuil: seuil_ecretage(),
        }
    }

    pub fn canaux(&self) -> usize {
        self.canaux
    }

    /// Pousse des trames entrelacées (`canaux` valeurs par trame).
    pub fn pousser(&mut self, entrelaces: &[f32]) {
        for trame in entrelaces.chunks_exact(self.canaux) {
            for (c, &x) in trame.iter().enumerate() {
                self.courante[c].ajouter(x);
                let t = &mut self.totaux[c];
                let a = x.abs();
                t.crete = t.crete.max(a);
                t.somme_carres += f64::from(x) * f64::from(x);
                t.n += 1;
                if a >= self.seuil {
                    t.ecretes += 1;
                }
            }
            self.trames += 1;
            self.trames_courante += 1;
            if self.trames_courante >= self.taille_case {
                self.clore_la_case();
            }
        }
    }

    fn clore_la_case(&mut self) {
        for c in 0..self.canaux {
            let case = std::mem::replace(&mut self.courante[c], Case::VIDE);
            self.cases[c].push(case);
        }
        self.trames_courante = 0;
        if self.cases[0].len() >= 4 * self.colonnes {
            // Mémoire bornée : deux voisines fusionnent, les cases doublent.
            for voie in &mut self.cases {
                let fusion: Vec<Case> = voie
                    .chunks(2)
                    .map(|p| {
                        let mut a = p[0];
                        if let Some(b) = p.get(1) {
                            a.fusionner(b);
                        }
                        a
                    })
                    .collect();
                *voie = fusion;
            }
            self.taille_case *= 2;
        }
    }

    pub fn terminer(mut self) -> FormeOnde {
        if self.trames_courante > 0 {
            self.clore_la_case();
        }
        let m = self.cases[0].len();
        let colonnes = self.colonnes.min(m);
        let voies = self
            .cases
            .iter()
            .zip(&self.totaux)
            .map(|(cases, t)| {
                let mut min = Vec::with_capacity(colonnes);
                let mut max = Vec::with_capacity(colonnes);
                let mut rms = Vec::with_capacity(colonnes);
                for j in 0..colonnes {
                    let debut = j * m / colonnes;
                    let fin = ((j + 1) * m / colonnes).max(debut + 1);
                    let mut col = Case::VIDE;
                    for case in &cases[debut..fin] {
                        col.fusionner(case);
                    }
                    min.push(col.min);
                    max.push(col.max);
                    rms.push(col.rms() as f32);
                }
                Voie {
                    min,
                    max,
                    rms,
                    crete: f64::from(t.crete),
                    rms_globale: if t.n == 0 {
                        0.0
                    } else {
                        (t.somme_carres / t.n as f64).sqrt()
                    },
                    ecretes: t.ecretes,
                }
            })
            .collect::<Vec<_>>();
        let n: u64 = self.totaux.iter().map(|t| t.n).sum();
        let somme: f64 = self.totaux.iter().map(|t| t.somme_carres).sum();
        FormeOnde {
            sample_rate: self.sample_rate,
            duree_s: self.trames as f64 / f64::from(self.sample_rate),
            trames: self.trames,
            colonnes,
            crete: voies.iter().map(|v| v.crete).fold(0.0, f64::max),
            rms_globale: if n == 0 {
                0.0
            } else {
                (somme / n as f64).sqrt()
            },
            ecretes: voies.iter().map(|v| v.ecretes).sum(),
            voies,
        }
    }
}

/// Un canal : ses colonnes (min, max, RMS, linéaires) et ses chiffres.
#[derive(Debug, Clone)]
pub struct Voie {
    pub min: Vec<f32>,
    pub max: Vec<f32>,
    pub rms: Vec<f32>,
    /// Crête maximale, linéaire (0..1).
    pub crete: f64,
    /// RMS globale, linéaire.
    pub rms_globale: f64,
    pub ecretes: u64,
}

impl Voie {
    pub fn crete_dbfs(&self) -> Option<f64> {
        dbfs(self.crete)
    }
    pub fn rms_dbfs(&self) -> Option<f64> {
        dbfs(self.rms_globale)
    }
    /// Facteur de crête (PLR), en dB.
    pub fn facteur_de_crete_db(&self) -> Option<f64> {
        Some(self.crete_dbfs()? - self.rms_dbfs()?)
    }
}

#[derive(Debug, Clone)]
pub struct FormeOnde {
    pub sample_rate: u32,
    pub duree_s: f64,
    pub trames: u64,
    pub colonnes: usize,
    pub voies: Vec<Voie>,
    /// Tous canaux confondus.
    pub crete: f64,
    pub rms_globale: f64,
    pub ecretes: u64,
}

impl FormeOnde {
    pub fn crete_dbfs(&self) -> Option<f64> {
        dbfs(self.crete)
    }
    pub fn rms_dbfs(&self) -> Option<f64> {
        dbfs(self.rms_globale)
    }
    pub fn facteur_de_crete_db(&self) -> Option<f64> {
        Some(self.crete_dbfs()? - self.rms_dbfs()?)
    }
}

/// Des échantillons entiers sur `bits` bits, normalisés à ±1.
pub fn normaliser(echantillons: &[i32], bits: u32) -> Vec<f32> {
    let echelle = 1.0 / (1u64 << bits.clamp(1, 32).saturating_sub(1)) as f32;
    echantillons
        .iter()
        .map(|&s| (s as f32 * echelle).clamp(-1.0, 1.0))
        .collect()
}

/// Calcule la forme d'onde du fichier `chemin`, de `debut_s` sur `duree_s`
/// secondes (toute la piste quand `None`). `duree_piste_s` : la durée connue
/// de la piste, pour borner le décodage et dimensionner les cases. `ceder`
/// est appelé entre deux fenêtres.
pub fn calculer(
    chemin: &str,
    colonnes: usize,
    debut_s: f64,
    duree_s: Option<f64>,
    duree_piste_s: Option<f64>,
    ceder: &dyn Fn(),
) -> Result<FormeOnde, String> {
    let debut_s = debut_s.max(0.0);
    let longueur = match (duree_s, duree_piste_s) {
        (Some(d), _) if d > 0.0 => Some(d),
        (_, Some(p)) if p > debut_s => Some(p - debut_s),
        _ => None,
    };
    let fin_s = longueur.map(|l| debut_s + l);
    // Six heures au plus sans durée connue.
    let fenetres_max = match longueur {
        Some(l) => (l / FENETRE_S).ceil() as usize + 1,
        None => (6.0 * 3600.0 / FENETRE_S) as usize,
    };
    let mut accumulateur: Option<Accumulateur> = None;
    for i in 0..fenetres_max {
        let depart = debut_s + i as f64 * FENETRE_S;
        let mut fenetre = FENETRE_S;
        if let Some(fin) = fin_s {
            if depart >= fin - 1e-6 {
                break;
            }
            fenetre = fenetre.min(fin - depart);
        }
        if i > 0 {
            ceder();
        }
        let pcm = match crate::audio::decode::decode_to_pcm(chemin, None, None, depart, fenetre) {
            Ok(pcm) => pcm,
            // Après la première fenêtre, une erreur est la fin du fichier.
            Err(_) if accumulateur.is_some() => break,
            Err(e) => return Err(e),
        };
        if pcm.samples_i32.is_empty() {
            break;
        }
        let canaux = pcm.channels.max(1);
        let acc = accumulateur.get_or_insert_with(|| {
            let estimees = longueur.map(|l| (l * f64::from(pcm.sample_rate)) as u64);
            Accumulateur::new(pcm.sample_rate, canaux, colonnes, estimees)
        });
        if acc.canaux() != canaux as usize {
            return Err("nombre de canaux variable".to_string());
        }
        acc.pousser(&normaliser(&pcm.samples_i32, u32::from(pcm.bit_depth)));
        let trames = pcm.samples_i32.len() / canaux as usize;
        // Une fenêtre incomplète est la dernière.
        if (trames as f64) < fenetre * f64::from(pcm.sample_rate) * 0.99 {
            break;
        }
    }
    let Some(accumulateur) = accumulateur else {
        return Err("aucun échantillon décodé".to_string());
    };
    Ok(accumulateur.terminer())
}

#[cfg(test)]
#[path = "forme_onde_tests_2793.rs"]
mod tests;
