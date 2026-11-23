//! TESS T11b — modulateur sigma-delta : PCM `f64` → DSD64, DSD128 ou DSD256.
//!
//! # La chaîne, dans l'ordre
//!
//! 1. **Suréchantillonnage** PCM → cadence DSD ([`interpolation`]), en `f64` ;
//! 2. **volume juste avant le modulateur** : c'est l'exception DSD de la
//!    décision Q9 (17/11). En PCM le volume est au bout de la chaîne, au
//!    rendu ; en DSD il n'existe plus d'échantillon PCM au rendu, donc le
//!    volume s'applique ici, à la cadence DSD, avec une rampe linéaire
//!    ([`RAMPE_VOLUME_S`]) pour ne jamais faire de marche ;
//! 3. **niveau de référence** : 0 dBFS PCM = 50 % de modulation
//!    ([`NIVEAU_REFERENCE`]), la convention du « 0 dB SACD » ;
//! 4. **limiteur de sécurité** à ±[`LIMITE_MODULATION`] (+1,6 dB au-dessus du
//!    0 dB SACD) : une crête intersample née du suréchantillonnage, ou un
//!    étage amont qui déborde, ne peut pas pousser la boucle hors de sa zone
//!    stable ;
//! 5. **boucle sigma-delta à 1 bit** d'ordre 7, structure à rétroaction de
//!    l'erreur : `v = x + (NTF − 1)·e`, `y = signe(v)`, `e = y − v`, d'où
//!    `y = x + NTF·e` (STF = 1). Le NTF ([`conception`]) a ses zéros
//!    optimisés dans la bande 0 – 22,05 kHz et un gain hors bande de
//!    [`HINF`] ;
//! 6. **dither TPDF** de ±[`DITHER`] devant le quantificateur, mis en forme
//!    par la boucle avec l'erreur : il casse les tons de repos d'un
//!    modulateur à 1 bit. Tirage déterministe (même entrée, mêmes bits) ;
//! 7. **garde d'instabilité** : si l'entrée du quantificateur dépasse
//!    [`SEUIL_INSTABILITE`], la boucle est remise à zéro et l'événement est
//!    compté ([`ModulateurDsd::reinitialisations`]). En marche normale il ne
//!    se produit jamais (témoins de stabilité) ; c'est le dernier recours, pas
//!    le mécanisme de stabilité ;
//! 8. **emballage** : 8 bits DSD par octet, **bit de poids fort = le plus
//!    ancien** (convention DFF et DoP), octets entrelacés par canal.
//!
//! # Valeurs retenues, et pourquoi
//!
//! Mesurées sur le prototype avant d'être figées (le banc les rejoue,
//! `examples/banc_modulateur_dsd.rs`) : ordre 7 et `HINF` = 1,5 donnent
//! 3 dB de bruit en moins mais décrochent dès 60 % de modulation ; `HINF` =
//! 1,3 reste stable au limiteur (60 %).
//!
//! Dither : ±0,1 laissait des tons de repos sur une entrée continue faible
//! (raie à −125,7 dB pour un continu de 3e-3, −121,9 dB sans dither) ; ±0,2
//! les ramène à −136,6 dB. Le prix, mesuré au banc : le plancher sur silence
//! passe de −126,4 à −121,5 dB en DSD64 (−170,9 à −165,9 dB en DSD128,
//! inchangé en DSD256), et l'entrée du quantificateur monte (≤ 1,06 en DSD64,
//! ≤ 1,48 en DSD256 sur la crête intersample tenue à 60 %), toujours loin du
//! seuil d'instabilité ([`SEUIL_INSTABILITE`]) : aucune remise à zéro.
//!
//! Aucune allocation dans [`ModulateurDsd::traiter`].

pub mod conception;
pub mod interpolation;
/// Outils de mesure des témoins et du banc : hors de l'API publique du SDK,
/// compilés pour les tests ou avec la fonctionnalité `mesure-dsd` (banc).
#[cfg(any(test, feature = "mesure-dsd"))]
pub mod mesure;

pub use conception::Ntf;
pub use interpolation::Interpolateur;

/// Ordre de la boucle.
pub const ORDRE: usize = 7;
/// Gain maximal hors bande du NTF (critère de Lee).
pub const HINF: f64 = 1.3;
/// 0 dBFS PCM = 50 % de modulation (0 dB SACD).
pub const NIVEAU_REFERENCE: f64 = 0.5;
/// Plafond de modulation à l'entrée de la boucle.
pub const LIMITE_MODULATION: f64 = 0.6;
/// Amplitude crête du dither TPDF devant le quantificateur.
pub const DITHER: f64 = 0.2;
/// Au-delà, la boucle est jugée instable et remise à zéro.
pub const SEUIL_INSTABILITE: f64 = 4.0;
/// Durée de la rampe de volume.
pub const RAMPE_VOLUME_S: f64 = 0.010;
/// Bande utile de conception du NTF, en Hz.
pub const BANDE_UTILE_HZ: f64 = 22_050.0;

/// Cadence DSD visée, en multiple de 44,1 kHz (famille 44,1) ou de 48 kHz
/// (famille 48).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadenceDsd {
    Dsd64,
    Dsd128,
    Dsd256,
}

impl CadenceDsd {
    pub fn multiple(self) -> u32 {
        match self {
            Self::Dsd64 => 64,
            Self::Dsd128 => 128,
            Self::Dsd256 => 256,
        }
    }

    /// Cadence DSD pour une entrée PCM à `cadence_pcm` Hz : famille 44,1 kHz
    /// (2,8224 MHz pour DSD64) ou famille 48 kHz (3,072 MHz). `None` si la
    /// cadence PCM n'est pas 44,1 ou 48 kHz × 2ⁿ, ou si le rapport n'est pas
    /// une puissance de deux d'au moins 8.
    pub fn cadence_pour(self, cadence_pcm: u32) -> Option<u32> {
        let base = [44_100_u32, 48_000]
            .into_iter()
            .find(|b| cadence_pcm.is_multiple_of(*b) && (cadence_pcm / b).is_power_of_two())?;
        let sortie = base.checked_mul(self.multiple())?;
        (sortie.is_multiple_of(cadence_pcm) && (sortie / cadence_pcm) >= 8).then_some(sortie)
    }
}

/// Erreur de construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurModulateur {
    CadenceNonPrise(u32, CadenceDsd),
    CanauxInvalides(u16),
    Conception,
    /// L'entrée n'est pas faite de trames entières (`échantillons`, `canaux`).
    TramesIncompletes {
        echantillons: usize,
        canaux: usize,
    },
    /// Le tampon de sortie est trop court (`besoin`, `recu` octets).
    TamponTropCourt {
        besoin: usize,
        recu: usize,
    },
}

impl std::fmt::Display for ErreurModulateur {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CadenceNonPrise(r, c) => write!(
                f,
                "PCM rate {r} Hz cannot be modulated to {c:?} (44.1 or 48 kHz × 2^n, ratio ≥ 8)"
            ),
            Self::CanauxInvalides(n) => write!(f, "invalid channel count {n}"),
            Self::Conception => write!(f, "noise transfer function design failed"),
            Self::TramesIncompletes {
                echantillons,
                canaux,
            } => write!(
                f,
                "{echantillons} samples are not whole {canaux}-channel frames"
            ),
            Self::TamponTropCourt { besoin, recu } => {
                write!(f, "output buffer holds {recu} bytes, {besoin} needed")
            }
        }
    }
}

impl std::error::Error for ErreurModulateur {}

/// Générateur déterministe (xorshift64*), un par canal.
#[derive(Debug, Clone)]
struct Alea(u64);

impl Alea {
    #[inline]
    fn suivant(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniforme sur [0, 1).
    #[cfg_attr(not(test), allow(dead_code))]
    fn uniforme(&mut self) -> f64 {
        ((self.suivant() >> 11) as f64) * (1.0 / (1u64 << 53) as f64)
    }

    /// Triangulaire (TPDF) sur ]−1, 1[ : différence de deux uniformes de
    /// 32 bits, tirées d'un seul tirage de 64 bits.
    #[inline]
    fn triangulaire(&mut self) -> f64 {
        let x = self.suivant();
        let echelle = 1.0 / 4_294_967_296.0;
        ((x >> 32) as f64 - (x & 0xffff_ffff) as f64) * echelle
    }
}

/// La boucle sigma-delta d'un canal. Tableaux de taille fixe ([`ORDRE`]) :
/// le compilateur déroule la forme directe.
#[derive(Debug, Clone)]
pub struct Boucle {
    /// Coefficients de N − D (sans le terme de tête, nul), puis de D.
    g: [f64; ORDRE],
    d: [f64; ORDRE],
    etat: [f64; ORDRE],
    alea: Alea,
    /// État initial du générateur, rendu par [`Self::reinitialiser`].
    graine: u64,
    dither: f64,
    pub entree_max: f64,
    pub reinitialisations: u64,
}

impl Boucle {
    /// `ntf` doit être d'ordre [`ORDRE`].
    pub fn new(ntf: &Ntf, graine: u64, dither: f64) -> Self {
        assert_eq!(ntf.ordre, ORDRE, "boucle d'ordre {ORDRE}");
        let mut g = [0.0; ORDRE];
        let mut d = [0.0; ORDRE];
        let (n, den) = (&ntf.numerateur[1..], &ntf.denominateur[1..]);
        for (((gk, dk), nk), dnk) in g.iter_mut().zip(d.iter_mut()).zip(n).zip(den) {
            *gk = nk - dnk;
            *dk = *dnk;
        }
        Self {
            g,
            d,
            etat: [0.0; ORDRE],
            alea: Alea(graine | 1),
            graine: graine | 1,
            dither,
            entree_max: 0.0,
            reinitialisations: 0,
        }
    }

    /// Boucle et générateur du dither remis à l'état de la construction : la
    /// suite rend les mêmes bits qu'une boucle neuve.
    pub fn reinitialiser(&mut self) {
        self.etat = [0.0; ORDRE];
        self.alea = Alea(self.graine);
    }

    /// Un échantillon à la cadence DSD : rend le bit (`true` = +1).
    #[inline]
    pub fn pas(&mut self, x: f64) -> bool {
        let mut f = self.etat[0];
        let mut v = x + f;
        if v.is_nan() || v.abs() > SEUIL_INSTABILITE {
            // Boucle partie (ou NaN) : remise à zéro, comptée. Jamais de NaN
            // dans l'état.
            self.etat = [0.0; ORDRE];
            self.reinitialisations += 1;
            f = 0.0;
            v = if x.is_finite() { x } else { 0.0 };
        }
        let av = v.abs();
        if av > self.entree_max {
            self.entree_max = av;
        }
        let d = if self.dither > 0.0 {
            self.dither * self.alea.triangulaire()
        } else {
            0.0
        };
        let bit = v + d >= 0.0;
        let q = if bit { 1.0 } else { -1.0 };
        let e = q - v;
        // Forme directe II transposée de H = (N − D) / D, entrée e, sortie f.
        for k in 0..ORDRE - 1 {
            self.etat[k] = self.etat[k + 1] + self.g[k] * e - self.d[k] * f;
        }
        self.etat[ORDRE - 1] = self.g[ORDRE - 1] * e - self.d[ORDRE - 1] * f;
        bit
    }
}

/// Le modulateur complet d'un flux PCM entrelacé. Voir le module.
#[derive(Debug, Clone)]
pub struct ModulateurDsd {
    canaux: usize,
    cadence_pcm: u32,
    cadence_dsd: u32,
    rapport: usize,
    interpolateur: Interpolateur,
    boucles: Vec<Boucle>,
    ntf: Ntf,
    gain: f64,
    cible: f64,
    pas_rampe: f64,
    /// `rapport` gains, un par échantillon DSD de la trame en cours.
    gains: Vec<f64>,
    /// `rapport` échantillons à la cadence DSD.
    tampon: Vec<f64>,
}

impl ModulateurDsd {
    /// HORS TEMPS RÉEL. Prépare la modulation de `canaux` canaux à
    /// `cadence_pcm` Hz vers `cadence`.
    pub fn new(
        cadence_pcm: u32,
        canaux: u16,
        cadence: CadenceDsd,
    ) -> Result<Self, ErreurModulateur> {
        Self::avec_reglages(cadence_pcm, canaux, cadence, HINF, DITHER)
    }

    /// [`Self::new`] avec un `hinf` et un dither choisis (banc, contre-épreuves).
    pub fn avec_reglages(
        cadence_pcm: u32,
        canaux: u16,
        cadence: CadenceDsd,
        hinf: f64,
        dither: f64,
    ) -> Result<Self, ErreurModulateur> {
        if canaux == 0 || canaux > 8 {
            return Err(ErreurModulateur::CanauxInvalides(canaux));
        }
        let cadence_dsd = cadence
            .cadence_pour(cadence_pcm)
            .ok_or(ErreurModulateur::CadenceNonPrise(cadence_pcm, cadence))?;
        let rapport = (cadence_dsd / cadence_pcm) as usize;
        let osr = f64::from(cadence_dsd) / (2.0 * BANDE_UTILE_HZ);
        let ntf = Ntf::concevoir(ORDRE, osr, hinf).ok_or(ErreurModulateur::Conception)?;
        let interpolateur =
            Interpolateur::new(usize::from(canaux), rapport).ok_or(ErreurModulateur::Conception)?;
        let boucles = (0..u64::from(canaux))
            .map(|c| {
                Boucle::new(
                    &ntf,
                    0x9e37_79b9_7f4a_7c15 ^ (c + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9),
                    dither,
                )
            })
            .collect();
        Ok(Self {
            canaux: usize::from(canaux),
            cadence_pcm,
            cadence_dsd,
            rapport,
            interpolateur,
            boucles,
            ntf,
            gain: 1.0,
            cible: 1.0,
            pas_rampe: 1.0 / (RAMPE_VOLUME_S * f64::from(cadence_dsd)),
            gains: vec![1.0; rapport],
            tampon: vec![0.0; rapport],
        })
    }

    pub fn canaux(&self) -> usize {
        self.canaux
    }
    pub fn cadence_pcm(&self) -> u32 {
        self.cadence_pcm
    }
    pub fn cadence_dsd(&self) -> u32 {
        self.cadence_dsd
    }
    /// Échantillons DSD par trame PCM.
    pub fn rapport(&self) -> usize {
        self.rapport
    }
    pub fn ntf(&self) -> &Ntf {
        &self.ntf
    }
    /// Retard du suréchantillonneur, en échantillons DSD.
    pub fn retard_dsd(&self) -> f64 {
        self.interpolateur.retard()
    }

    /// Octets DSD produits pour `trames` trames PCM (tous canaux).
    pub fn octets_pour(&self, trames: usize) -> usize {
        trames * self.rapport / 8 * self.canaux
    }

    /// Volume linéaire (1,0 = 0 dB). Atteint en [`RAMPE_VOLUME_S`] au plus,
    /// sans marche ; un volume non fini ou négatif est ignoré.
    pub fn definir_volume(&mut self, gain: f64) {
        if gain.is_finite() && gain >= 0.0 {
            // Un volume ne dépasse jamais 0 dB : au-delà, ce serait du gain,
            // que le limiteur de sécurité écrêterait.
            self.cible = gain.min(1.0);
        }
    }

    pub fn volume(&self) -> f64 {
        self.gain
    }

    /// Nombre de remises à zéro de la boucle (instabilités), tous canaux.
    pub fn reinitialisations(&self) -> u64 {
        self.boucles.iter().map(|b| b.reinitialisations).sum()
    }

    /// Plus grande entrée du quantificateur vue, tous canaux.
    pub fn entree_max(&self) -> f64 {
        self.boucles
            .iter()
            .map(|b| b.entree_max)
            .fold(0.0, f64::max)
    }

    /// Nouvelle piste ou saut : historiques, boucles et générateurs du dither
    /// à l'état de la construction ; le volume et sa rampe sont GARDÉS (aucune
    /// marche de niveau). Après une remise à zéro, la même entrée rend les
    /// mêmes bits qu'un modulateur neuf réglé au même volume.
    pub fn reinitialiser(&mut self) {
        self.interpolateur.reinitialiser();
        for b in &mut self.boucles {
            b.reinitialiser();
        }
    }

    /// TEMPS RÉEL. Module `entree` (PCM entrelacé, pleine échelle ±1,0,
    /// trames entières) dans `sortie`, qui doit tenir
    /// [`Self::octets_pour`]`(trames)` octets. Rend le nombre d'octets écrits.
    pub fn traiter(
        &mut self,
        entree: &[f64],
        sortie: &mut [u8],
    ) -> Result<usize, ErreurModulateur> {
        let c = self.canaux;
        if !entree.len().is_multiple_of(c) {
            return Err(ErreurModulateur::TramesIncompletes {
                echantillons: entree.len(),
                canaux: c,
            });
        }
        let trames = entree.len() / c;
        let besoin = self.octets_pour(trames);
        if sortie.len() < besoin {
            return Err(ErreurModulateur::TamponTropCourt {
                besoin,
                recu: sortie.len(),
            });
        }
        let octets_par_canal = self.rapport / 8;
        for (t, trame) in entree.chunks_exact(c).enumerate() {
            // Gains de la trame : la rampe avance d'un pas par échantillon DSD.
            for g in &mut self.gains {
                let ecart = self.cible - self.gain;
                if ecart.abs() <= self.pas_rampe {
                    self.gain = self.cible;
                } else {
                    self.gain += self.pas_rampe.copysign(ecart);
                }
                *g = self.gain * NIVEAU_REFERENCE;
            }
            self.interpolateur.pousser(trame);
            let base = t * octets_par_canal * c;
            for canal in 0..c {
                self.interpolateur.produire(canal, &mut self.tampon);
                let boucle = &mut self.boucles[canal];
                for (o, (morceau, gains)) in self
                    .tampon
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .zip(self.gains.as_chunks::<8>().0)
                    .enumerate()
                {
                    let mut octet = 0u8;
                    for (x, g) in morceau.iter().zip(gains) {
                        let s = (x * g).clamp(-LIMITE_MODULATION, LIMITE_MODULATION);
                        octet = (octet << 1) | u8::from(boucle.pas(s));
                    }
                    sortie[base + o * c + canal] = octet;
                }
            }
        }
        Ok(besoin)
    }
}

#[cfg(test)]
mod tests;
