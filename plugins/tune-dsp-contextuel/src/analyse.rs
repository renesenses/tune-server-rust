//! Mesures de fichier, hors ligne, sur un tampon PCM `f64` entrelacé
//! (réutilisables à la numérisation d'un vinyle ou d'un CD).
//!
//! [`Analyseur`] se nourrit par blocs de taille quelconque ; le résultat ne
//! dépend pas du découpage. Les mesures inter-canaux (corrélation, polarité,
//! écart L/R, double-mono) ne valent que pour la stéréo.
//!
//! La crête vraie est celle de `tune_core::audio::crete_vraie` (BS.1770),
//! réutilisée telle quelle.
//!
//! Plus bas : la lecture de l'indicateur de pré-accentuation (TOC, CUE) et la
//! [`SondeDeNiveau`] dont le comparatif A/B à niveau égal aura besoin.

use serde::{Deserialize, Serialize};
use tune_core::audio::crete_vraie::CreteVraie;

use crate::traitements::filtres::{Cascade, Coefs, Q_BUTTERWORTH_2};

/// Seuils de déclenchement des règles par défaut (voir le contrat, §5).
pub mod seuils {
    /// Composante continue au-dessus de laquelle on propose le passe-haut.
    pub const CONTINU_DBFS: f64 = -60.0;
    // Polarité RELATIVE inversée entre canaux : les QUATRE critères à la
    // fois (une stéréo très large ou décorrélée ne doit jamais être signalée).
    /// 1. Corrélation L/R sur tout le fichier, au plus.
    pub const CORRELATION_INVERSEE: f64 = -0.6;
    /// 2. La somme plus faible que la différence d'au moins ce rapport.
    pub const RAPPORT_SOMME_DIFFERENCE_DB: f64 = 6.0;
    /// 3. Corrélation des GRAVES (passe-bas à [`GRAVES_HZ`]), au plus : les
    ///    graves d'un mixage sont presque toujours au centre ; une stéréo
    ///    élargie garde des graves corrélés, une inversion les inverse aussi.
    pub const CORRELATION_GRAVES_INVERSEE: f64 = -0.6;
    pub const GRAVES_HZ: f64 = 250.0;
    /// 4. Part des blocs de [`BLOC_POLARITE_S`] non silencieux dont la
    ///    corrélation est au plus [`CORRELATION_BLOC_INVERSEE`] : l'inversion
    ///    tient tout le fichier, pas un passage.
    pub const PART_BLOCS_INVERSES: f64 = 0.75;
    pub const CORRELATION_BLOC_INVERSEE: f64 = -0.3;
    pub const BLOC_POLARITE_S: f64 = 1.0;
    /// En dessous, le signal est jugé silencieux : pas de verdict inter-canaux.
    pub const SILENCE_DBFS: f64 = -60.0;
    /// Énergie de la différence sous la somme, pour dire « double-mono ».
    pub const DOUBLE_MONO_DB: f64 = 60.0;
    /// Écart L/R à partir duquel l'équilibre est proposé.
    pub const ECART_LR_DB: f64 = 0.5;
    /// Durée d'un bloc de l'estimation du DR, en secondes.
    pub const BLOC_DR_S: f64 = 3.0;
}

/// Ce que l'analyse d'un fichier (ou d'un album) a mesuré.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MesuresFichier {
    pub cadence: u32,
    pub canaux: u16,
    pub trames: u64,
    /// Composante continue par canal, en dBFS ([`PLANCHER_DB`] si nulle).
    pub continu_dbfs: Vec<f64>,
    /// Corrélation de Pearson L/R ; `None` hors stéréo ou sur un silence.
    pub correlation: Option<f64>,
    /// Somme/différence, en dB (`10·log10(E(L+R) / E(L−R))`).
    pub rapport_somme_difference_db: Option<f64>,
    /// Corrélation L/R des graves (sous [`seuils::GRAVES_HZ`]).
    #[serde(default)]
    pub correlation_graves: Option<f64>,
    /// Part des blocs d'une seconde à corrélation nettement négative.
    #[serde(default)]
    pub part_blocs_inverses: Option<f64>,
    /// Polarité RELATIVE inversée entre les canaux (jamais absolue : aucune
    /// mesure ne dit si le programme entier est inversé).
    pub polarite_inversee: bool,
    /// Niveau gauche − niveau droit, en dB.
    pub ecart_lr_db: Option<f64>,
    pub double_mono: bool,
    pub crete_vraie_dbtp: f64,
    /// Estimation façon DR Meter (voir [`Analyseur`]) ; `None` sur un silence.
    pub dr: Option<f64>,
    /// RMS de tout le fichier, tous canaux, en dBFS.
    pub niveau_rms_dbfs: f64,
}

impl MesuresFichier {
    /// Le pire canal pour la composante continue.
    pub fn continu_max_dbfs(&self) -> f64 {
        self.continu_dbfs
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }
}

/// Plancher des niveaux en dB : un silence vaut −300 dB plutôt que `-inf`,
/// que JSON ne sait pas écrire (les mesures se gardent en base).
pub const PLANCHER_DB: f64 = -300.0;

fn db_puissance(x: f64) -> f64 {
    (10.0 * x.log10()).max(PLANCHER_DB)
}

fn db_amplitude(x: f64) -> f64 {
    (20.0 * x.abs().log10()).max(PLANCHER_DB)
}

/// Accumulateur de mesures (voir le module).
///
/// Estimation du DR : par canal, blocs de 3 s ; pour chacun, RMS × √2 et
/// crête d'échantillon ; RMS₂₀ = moyenne quadratique des 20 % de blocs les
/// plus forts ; crête₂ = deuxième plus grande crête de bloc (la plus grande
/// s'il n'y a qu'un bloc) ; DR = moyenne sur les canaux de
/// `20·log10(crête₂ / RMS₂₀)`. Un dernier bloc incomplet ne compte que s'il
/// est le seul.
pub struct Analyseur {
    cadence: u32,
    canaux: usize,
    trames: u64,
    somme: Vec<f64>,
    somme_carres: Vec<f64>,
    somme_lr: f64,
    energie_somme: f64,
    energie_difference: f64,
    crete: CreteVraie,
    trames_par_bloc: u64,
    bloc_trames: u64,
    bloc_carres: Vec<f64>,
    bloc_crete: Vec<f64>,
    /// Par canal : (RMS × √2, crête) des blocs complets.
    blocs: Vec<Vec<(f64, f64)>>,
    polarite: Option<DetecteurDePolarite>,
}

/// Les critères 3 et 4 de la polarité (voir [`seuils`]), stéréo seulement.
struct DetecteurDePolarite {
    graves: Cascade,
    /// Σl, Σr, Σl², Σr², Σlr des graves.
    sommes_graves: [f64; 5],
    trames_par_bloc: u64,
    bloc_trames: u64,
    /// Σl², Σr², Σlr du bloc courant.
    bloc: [f64; 3],
    blocs_actifs: u64,
    blocs_inverses: u64,
}

fn correlation_de(n: f64, s: [f64; 5]) -> Option<f64> {
    let (ml, mr) = (s[0] / n, s[1] / n);
    let vl = s[2] / n - ml * ml;
    let vr = s[3] / n - mr * mr;
    let cov = s[4] / n - ml * mr;
    (vl > 0.0 && vr > 0.0).then(|| (cov / (vl * vr).sqrt()).clamp(-1.0, 1.0))
}

impl DetecteurDePolarite {
    fn new(cadence: u32) -> Self {
        Self {
            graves: Cascade::new(
                vec![Coefs::passe_bas(
                    seuils::GRAVES_HZ,
                    f64::from(cadence.max(1)),
                    Q_BUTTERWORTH_2,
                )],
                2,
            ),
            sommes_graves: [0.0; 5],
            trames_par_bloc: ((f64::from(cadence) * seuils::BLOC_POLARITE_S) as u64).max(1),
            bloc_trames: 0,
            bloc: [0.0; 3],
            blocs_actifs: 0,
            blocs_inverses: 0,
        }
    }

    fn nourrir(&mut self, l: f64, r: f64) {
        let (gl, gr) = (self.graves.traiter(0, l), self.graves.traiter(1, r));
        let s = &mut self.sommes_graves;
        s[0] += gl;
        s[1] += gr;
        s[2] += gl * gl;
        s[3] += gr * gr;
        s[4] += gl * gr;
        self.bloc[0] += l * l;
        self.bloc[1] += r * r;
        self.bloc[2] += l * r;
        self.bloc_trames += 1;
        if self.bloc_trames == self.trames_par_bloc {
            self.clore_le_bloc();
        }
    }

    fn compter(&self, bloc: [f64; 3], n: u64) -> (u64, u64) {
        let rms = ((bloc[0] + bloc[1]) / (2.0 * n.max(1) as f64)).sqrt();
        if db_amplitude(rms) <= seuils::SILENCE_DBFS || bloc[0] == 0.0 || bloc[1] == 0.0 {
            return (0, 0);
        }
        let c = bloc[2] / (bloc[0] * bloc[1]).sqrt();
        (1, u64::from(c <= seuils::CORRELATION_BLOC_INVERSEE))
    }

    fn clore_le_bloc(&mut self) {
        let (a, i) = self.compter(self.bloc, self.bloc_trames);
        self.blocs_actifs += a;
        self.blocs_inverses += i;
        self.bloc = [0.0; 3];
        self.bloc_trames = 0;
    }

    /// (corrélation des graves, part des blocs inversés), sur `trames`.
    fn resultat(&self, trames: u64) -> (Option<f64>, Option<f64>) {
        let s = self.sommes_graves;
        let n = trames.max(1) as f64;
        let rms_graves = ((s[2] + s[3]) / (2.0 * n)).sqrt();
        let graves = if db_amplitude(rms_graves) > seuils::SILENCE_DBFS {
            correlation_de(n, s)
        } else {
            None
        };
        let (mut actifs, mut inverses) = (self.blocs_actifs, self.blocs_inverses);
        if actifs == 0 && self.bloc_trames > 0 {
            (actifs, inverses) = self.compter(self.bloc, self.bloc_trames);
        }
        let part = (actifs > 0).then(|| inverses as f64 / actifs as f64);
        (graves, part)
    }
}

impl Analyseur {
    pub fn new(cadence: u32, canaux: u16) -> Self {
        let c = usize::from(canaux.max(1));
        Self {
            cadence,
            canaux: c,
            trames: 0,
            somme: vec![0.0; c],
            somme_carres: vec![0.0; c],
            somme_lr: 0.0,
            energie_somme: 0.0,
            energie_difference: 0.0,
            crete: CreteVraie::new(cadence as usize, c),
            trames_par_bloc: ((f64::from(cadence) * seuils::BLOC_DR_S) as u64).max(1),
            bloc_trames: 0,
            bloc_carres: vec![0.0; c],
            bloc_crete: vec![0.0; c],
            blocs: vec![Vec::new(); c],
            polarite: (c == 2).then(|| DetecteurDePolarite::new(cadence)),
        }
    }

    /// Nourrit des trames entrelacées complètes.
    pub fn nourrir(&mut self, entrelaces: &[f64]) {
        let c = self.canaux;
        let complet = entrelaces.len() / c * c;
        let entrelaces = &entrelaces[..complet];
        self.crete.nourrir(entrelaces);
        for t in entrelaces.chunks_exact(c) {
            for (k, &x) in t.iter().enumerate() {
                self.somme[k] += x;
                self.somme_carres[k] += x * x;
                self.bloc_carres[k] += x * x;
                self.bloc_crete[k] = self.bloc_crete[k].max(x.abs());
            }
            if c == 2 {
                self.somme_lr += t[0] * t[1];
                let (s, d) = (t[0] + t[1], t[0] - t[1]);
                self.energie_somme += s * s;
                self.energie_difference += d * d;
                if let Some(p) = self.polarite.as_mut() {
                    p.nourrir(t[0], t[1]);
                }
            }
            self.trames += 1;
            self.bloc_trames += 1;
            if self.bloc_trames == self.trames_par_bloc {
                self.clore_le_bloc();
            }
        }
    }

    fn clore_le_bloc(&mut self) {
        let n = self.bloc_trames as f64;
        for k in 0..self.canaux {
            let rms = (2.0 * self.bloc_carres[k] / n).sqrt();
            self.blocs[k].push((rms, self.bloc_crete[k]));
            self.bloc_carres[k] = 0.0;
            self.bloc_crete[k] = 0.0;
        }
        self.bloc_trames = 0;
    }

    fn dr(&self) -> Option<f64> {
        let mut total = 0.0;
        for k in 0..self.canaux {
            let mut blocs = self.blocs[k].clone();
            if blocs.is_empty() && self.bloc_trames > 0 {
                let n = self.bloc_trames as f64;
                blocs.push(((2.0 * self.bloc_carres[k] / n).sqrt(), self.bloc_crete[k]));
            }
            if blocs.is_empty() {
                return None;
            }
            let mut rms: Vec<f64> = blocs.iter().map(|b| b.0).collect();
            rms.sort_by(|a, b| b.total_cmp(a));
            let n20 = ((rms.len() as f64 * 0.2).round() as usize).max(1);
            let rms20 = (rms[..n20].iter().map(|r| r * r).sum::<f64>() / n20 as f64).sqrt();
            let mut cretes: Vec<f64> = blocs.iter().map(|b| b.1).collect();
            cretes.sort_by(|a, b| b.total_cmp(a));
            let crete2 = if cretes.len() > 1 {
                cretes[1]
            } else {
                cretes[0]
            };
            if rms20 == 0.0 || crete2 == 0.0 {
                return None;
            }
            total += db_amplitude(crete2 / rms20);
        }
        Some(total / self.canaux as f64)
    }

    pub fn resultat(&self) -> MesuresFichier {
        let n = self.trames.max(1) as f64;
        let continu_dbfs = self.somme.iter().map(|s| db_amplitude(s / n)).collect();
        let energie: f64 = self.somme_carres.iter().sum();
        let niveau_rms_dbfs = db_puissance(energie / (n * self.canaux as f64));
        let non_silencieux = niveau_rms_dbfs > seuils::SILENCE_DBFS;

        let (mut correlation, mut rapport, mut ecart) = (None, None, None);
        let (mut polarite_inversee, mut double_mono) = (false, false);
        let (mut correlation_graves, mut part_blocs_inverses) = (None, None);
        if self.canaux == 2 && self.trames > 0 && non_silencieux {
            correlation = correlation_de(
                n,
                [
                    self.somme[0],
                    self.somme[1],
                    self.somme_carres[0],
                    self.somme_carres[1],
                    self.somme_lr,
                ],
            );
            if let Some(p) = &self.polarite {
                (correlation_graves, part_blocs_inverses) = p.resultat(self.trames);
            }
            if self.somme_carres[0] > 0.0 && self.somme_carres[1] > 0.0 {
                ecart = Some(db_puissance(self.somme_carres[0] / self.somme_carres[1]));
            }
            double_mono = self.energie_difference
                <= self.energie_somme * 10f64.powf(-seuils::DOUBLE_MONO_DB / 10.0);
            if self.energie_difference > 0.0 {
                let r = db_puissance(self.energie_somme / self.energie_difference);
                rapport = Some(r);
                polarite_inversee = correlation.is_some_and(|c| c <= seuils::CORRELATION_INVERSEE)
                    && r <= -seuils::RAPPORT_SOMME_DIFFERENCE_DB
                    && correlation_graves.is_some_and(|c| c <= seuils::CORRELATION_GRAVES_INVERSEE)
                    && part_blocs_inverses.is_some_and(|p| p >= seuils::PART_BLOCS_INVERSES);
            } else {
                rapport = Some(-PLANCHER_DB);
            }
        }

        MesuresFichier {
            cadence: self.cadence,
            canaux: self.canaux as u16,
            trames: self.trames,
            continu_dbfs,
            correlation,
            correlation_graves,
            part_blocs_inverses,
            rapport_somme_difference_db: rapport,
            polarite_inversee,
            ecart_lr_db: ecart,
            double_mono,
            crete_vraie_dbtp: db_amplitude(self.crete.crete()),
            dr: self.dr(),
            niveau_rms_dbfs,
        }
    }
}

/// Le niveau d'un flux, pour le comparatif A/B à niveau égal (rc15) : on
/// nourrit une sonde avec le non-traité et une avec le traité, et
/// [`ecart_de_niveau_db`] dit de combien corriger le traité.
#[derive(Debug, Clone, Default)]
pub struct SondeDeNiveau {
    energie: f64,
    echantillons: u64,
}

impl SondeDeNiveau {
    pub fn nourrir(&mut self, entrelaces: &[f64]) {
        self.energie += entrelaces.iter().map(|x| x * x).sum::<f64>();
        self.echantillons += entrelaces.len() as u64;
    }

    /// Niveau RMS en dBFS ([`PLANCHER_DB`] sur un silence ou sans échantillon).
    pub fn niveau_dbfs(&self) -> f64 {
        if self.echantillons == 0 {
            return PLANCHER_DB;
        }
        db_puissance(self.energie / self.echantillons as f64)
    }

    fn silencieuse(&self) -> bool {
        self.energie == 0.0
    }
}

/// Le gain (dB) à appliquer au traité pour qu'il sonne au niveau du
/// non-traité. `0` si l'un des deux est silencieux.
pub fn ecart_de_niveau_db(non_traite: &SondeDeNiveau, traite: &SondeDeNiveau) -> f64 {
    if non_traite.silencieuse() || traite.silencieuse() {
        return 0.0;
    }
    non_traite.niveau_dbfs() - traite.niveau_dbfs()
}

/// Le bit de pré-accentuation du champ CONTROL d'une entrée de TOC ou du
/// sous-code Q (IEC 60908 : bit 0, valeur 0x01). `tune-cd` lit ce champ
/// (`cddafs.rs`, `linux.rs`) mais n'en garde aujourd'hui que le bit
/// « données ».
pub fn preaccentuation_du_controle(controle: u8) -> bool {
    controle & 0x01 != 0
}

/// Les numéros des pistes marquées `FLAGS … PRE` dans une feuille CUE.
/// `scanner::cue` (tune-core) ignore aujourd'hui les `FLAGS`.
pub fn pistes_preaccentuees_du_cue(feuille: &str) -> Vec<u32> {
    let mut courante: Option<u32> = None;
    let mut pistes = Vec::new();
    for ligne in feuille.lines() {
        let mut mots = ligne.split_whitespace();
        match mots.next().map(str::to_ascii_uppercase).as_deref() {
            Some("TRACK") => courante = mots.next().and_then(|n| n.parse().ok()),
            Some("FLAGS") => {
                if let Some(n) = courante
                    && mots.any(|m| m.eq_ignore_ascii_case("PRE"))
                    && !pistes.contains(&n)
                {
                    pistes.push(n);
                }
            }
            _ => {}
        }
    }
    pistes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traitements::essais::{bruit, rose, sinus};

    fn analyser(entree: &[f64], cadence: u32, canaux: u16, bloc: usize) -> MesuresFichier {
        let mut a = Analyseur::new(cadence, canaux);
        for m in entree.chunks(bloc * usize::from(canaux)) {
            a.nourrir(m);
        }
        a.resultat()
    }

    #[test]
    fn continu_mesure() {
        let mut e = sinus(1_000.0, 44_100.0, 0.5, 44_100, 2);
        e.as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] += 0.01);
        let m = analyser(&e, 44_100, 2, 4_096);
        assert!(m.continu_dbfs[0] < -100.0, "{:?}", m.continu_dbfs);
        // Un silence se mesure au plancher, jamais en `-inf` (JSON).
        let s = analyser(&vec![0.0; 2_000], 44_100, 2, 100);
        assert_eq!(s.continu_dbfs, vec![PLANCHER_DB; 2]);
        let relu: MesuresFichier =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(relu, s);
        assert!((m.continu_dbfs[1] + 40.0).abs() < 0.01);
        assert!((m.continu_max_dbfs() + 40.0).abs() < 0.01);
    }

    #[test]
    fn polarite_inversee_detectee_et_pas_sur_une_stereo_normale() {
        let mut inverse = bruit(44_100, 2, 0.5, 3);
        inverse
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] = -t[0] * 0.9 + t[1] * 0.1);
        let m = analyser(&inverse, 44_100, 2, 1_000);
        assert!(m.polarite_inversee, "{m:?}");
        assert!(m.correlation.unwrap() < -0.9);

        let mut normale = bruit(44_100, 2, 0.5, 3);
        normale
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] = t[0] * 0.6 + t[1] * 0.4);
        let m = analyser(&normale, 44_100, 2, 1_000);
        assert!(!m.polarite_inversee);
        // Une stéréo décorrélée (bruit indépendant) non plus.
        let m = analyser(&bruit(44_100, 2, 0.5, 8), 44_100, 2, 1_000);
        assert!(!m.polarite_inversee);
    }

    fn entrelacer(l: &[f64], r: &[f64]) -> Vec<f64> {
        l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect()
    }

    const DIX_S: usize = 10 * 44_100;

    /// Une vraie inversion relative, sur un programme « musical » (centre
    /// rose, côtés faibles) : signalée.
    #[test]
    fn polarite_relative_inversee_sur_un_programme_rose() {
        let m = rose(DIX_S, 1);
        let s = rose(DIX_S, 2);
        let l: Vec<f64> = m.iter().zip(&s).map(|(m, s)| m + 0.3 * s).collect();
        let r: Vec<f64> = m.iter().zip(&s).map(|(m, s)| -(m - 0.3 * s)).collect();
        let me = analyser(&entrelacer(&l, &r), 44_100, 2, 4_096);
        assert!(me.polarite_inversee, "{me:?}");
        assert!(me.correlation_graves.unwrap() < -0.8);
        assert_eq!(me.part_blocs_inverses, Some(1.0));
    }

    /// Contre-épreuve : des stéréos très décorrélées ou très larges ne sont
    /// JAMAIS signalées.
    #[test]
    fn stereos_decorrelees_jamais_signalees() {
        let mut cas: Vec<(&str, Vec<f64>)> = Vec::new();
        // Bruits roses indépendants.
        for graine in 1..=5u64 {
            cas.push((
                "roses indépendants",
                entrelacer(&rose(DIX_S, graine), &rose(DIX_S, graine + 50)),
            ));
        }
        // Orgue : dix partiels de do1, phases différentes par canal, canal
        // droit désaccordé de 0,37 Hz par partiel (battements lents).
        let orgue = |desaccord: f64, phase: f64| -> Vec<f64> {
            (0..DIX_S)
                .map(|n| {
                    let t = n as f64 / 44_100.0;
                    (1..=10)
                        .map(|k| {
                            let k = f64::from(k);
                            0.05 / k
                                * (2.0 * std::f64::consts::PI * (65.41 * k + desaccord * k) * t
                                    + phase * k * k)
                                    .sin()
                        })
                        .sum::<f64>()
                })
                .collect()
        };
        cas.push(("orgue", entrelacer(&orgue(0.0, 0.0), &orgue(0.37, 1.3))));
        // Ambiance : une source retardée de 13 ms et deux souffles
        // indépendants.
        let x = rose(DIX_S, 7);
        let (n1, n2) = (rose(DIX_S, 8), rose(DIX_S, 9));
        let l: Vec<f64> = (0..DIX_S).map(|i| x[i] + 0.5 * n1[i]).collect();
        let r: Vec<f64> = (0..DIX_S)
            .map(|i| if i >= 573 { x[i - 573] } else { 0.0 } + 0.5 * n2[i])
            .collect();
        cas.push(("ambiance", entrelacer(&l, &r)));
        // Stéréo élargie à l'extrême (côtés aigus 4× le centre) : corrélation
        // −0,74, somme 8 dB sous la différence, tous les blocs négatifs — seuls
        // les graves, restés au centre, la distinguent d'une inversion.
        let m = rose(DIX_S, 11);
        let mut cote = rose(DIX_S, 12);
        let mut hp = Cascade::new(vec![Coefs::passe_haut(500.0, 44_100.0, Q_BUTTERWORTH_2)], 1);
        cote.iter_mut().for_each(|x| *x = 4.0 * hp.traiter(0, *x));
        let l: Vec<f64> = m.iter().zip(&cote).map(|(m, s)| m + s).collect();
        let r: Vec<f64> = m.iter().zip(&cote).map(|(m, s)| m - s).collect();
        let large = entrelacer(&l, &r);
        let me = analyser(&large, 44_100, 2, 4_096);
        assert!(
            me.correlation.unwrap() < -0.6,
            "le cas doit rester piégeux : {me:?}"
        );
        cas.push(("stéréo élargie", large));

        for (nom, e) in cas {
            let me = analyser(&e, 44_100, 2, 4_096);
            assert!(!me.polarite_inversee, "{nom} signalé à tort : {me:?}");
        }
    }

    #[test]
    fn ecart_lr_et_double_mono() {
        let mut e = bruit(44_100, 2, 0.5, 5);
        e.as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] = t[0] * 10f64.powf(-1.5 / 20.0));
        let m = analyser(&e, 44_100, 2, 777);
        assert!((m.ecart_lr_db.unwrap() - 1.5).abs() < 1e-9);
        assert!(!m.double_mono);

        let mut dm = bruit(44_100, 2, 0.5, 5);
        dm.as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] = t[0]);
        let m = analyser(&dm, 44_100, 2, 777);
        assert!(m.double_mono);
        assert_eq!(m.ecart_lr_db, Some(0.0));
        // Un souffle indépendant à −70 dB sous le signal reste double-mono.
        let souffle = bruit(44_100, 2, 0.5 * 10f64.powf(-70.0 / 20.0), 99);
        let presque: Vec<f64> = dm.iter().zip(&souffle).map(|(a, b)| a + b).collect();
        assert!(analyser(&presque, 44_100, 2, 777).double_mono);
    }

    #[test]
    fn resultat_independant_du_decoupage() {
        let e = bruit(100_000, 2, 0.7, 21);
        let a = analyser(&e, 44_100, 2, 64);
        let b = analyser(&e, 44_100, 2, 50_000);
        assert_eq!(a.crete_vraie_dbtp, b.crete_vraie_dbtp);
        assert_eq!(a.double_mono, b.double_mono);
        assert_eq!(a.dr, b.dr);
        assert!((a.correlation.unwrap() - b.correlation.unwrap()).abs() < 1e-12);
    }

    #[test]
    fn crete_vraie_voit_l_over_inter_echantillons() {
        // fs/4 déphasé de 45° : échantillons à 0,707·A, crête vraie A.
        let mut e = Vec::new();
        for n in 0..44_100 {
            let x = (std::f64::consts::FRAC_PI_2 * n as f64 + std::f64::consts::FRAC_PI_4).sin();
            e.extend([x, x]);
        }
        let m = analyser(&e, 44_100, 2, 1_000);
        // Crête d'échantillon −3,01 dBFS ; la crête vraie de tune-core la
        // retrouve à 0 dBTP (+0,11 dB mesurés à fs/4 : ondulation de son
        // filtre d'interpolation, hors de cette caisse).
        assert!(m.crete_vraie_dbtp.abs() < 0.2, "{}", m.crete_vraie_dbtp);
    }

    #[test]
    fn dr_d_une_sinusoide_et_d_un_signal_a_pointes() {
        // Sinusoïde pure : RMS × √2 = crête, DR = 0.
        let s = sinus(1_000.0, 44_100.0, 0.5, 10 * 44_100, 2);
        let m = analyser(&s, 44_100, 2, 4_096);
        assert!(m.dr.unwrap().abs() < 0.05, "{:?}", m.dr);
        // Sinusoïde à 0,05 et une pointe à 1,0 par bloc de 3 s : ≈ 26 dB.
        let mut p = sinus(1_000.0, 44_100.0, 0.05, 12 * 44_100, 2);
        for b in 0..4 {
            let i = (b * 3 * 44_100 + 1_000) * 2;
            p[i] = 1.0;
            p[i + 1] = 1.0;
        }
        let dr = analyser(&p, 44_100, 2, 4_096).dr.unwrap();
        assert!((dr - 26.0).abs() < 0.2, "DR {dr}");
    }

    #[test]
    fn silence_sans_verdict() {
        let m = analyser(&vec![0.0; 20_000], 44_100, 2, 1_000);
        assert_eq!(m.correlation, None);
        assert!(!m.polarite_inversee && !m.double_mono);
        assert_eq!(m.dr, None);
    }

    #[test]
    fn sonde_de_niveau_pour_l_a_b() {
        let a = sinus(1_000.0, 44_100.0, 0.5, 44_100, 2);
        let b: Vec<f64> = a.iter().map(|x| x * 10f64.powf(-3.0 / 20.0)).collect();
        let (mut sa, mut sb) = (SondeDeNiveau::default(), SondeDeNiveau::default());
        sa.nourrir(&a);
        sb.nourrir(&b);
        assert!((ecart_de_niveau_db(&sa, &sb) - 3.0).abs() < 1e-9);
        assert_eq!(ecart_de_niveau_db(&sa, &SondeDeNiveau::default()), 0.0);
    }

    #[test]
    fn preaccentuation_toc_et_cue() {
        assert!(preaccentuation_du_controle(0x01));
        assert!(preaccentuation_du_controle(0x01 | 0x02));
        assert!(!preaccentuation_du_controle(0x00));
        assert!(!preaccentuation_du_controle(0x04));
        let cue = "FILE \"a.wav\" WAVE\n  TRACK 01 AUDIO\n    FLAGS DCP PRE\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 03:00:00\n  track 3 audio\n    flags pre\n";
        assert_eq!(pistes_preaccentuees_du_cue(cue), vec![1, 3]);
        assert!(pistes_preaccentuees_du_cue("TRACK 01 AUDIO\nFLAGS DCP\n").is_empty());
    }
}
