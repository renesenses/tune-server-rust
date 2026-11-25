//! Les neuf traitements de la vague 1 et ce qu'ils partagent.
//!
//! Règles (celles d'`EtageTess`, `tune-core/src/tess/etage.rs`) :
//!
//! - échantillons **entrelacés en `f64`**, pleine échelle ±1,0, traités **sur
//!   place** et **par blocs** ;
//! - [`Traitement::preparer`] est hors temps réel et alloue tout ;
//!   [`Traitement::traiter`] n'alloue jamais ;
//! - **bit-perfect** : un traitement inactif au repos, ou neutre, ne touche
//!   AUCUN échantillon (pas même une multiplication par 1,0) ;
//! - **jamais de saut** : un gain glisse en [`RAMPE_MS`] (la constante de
//!   TESS, lue et non redéfinie) ; activer ou couper un filtre est un fondu
//!   enchaîné sec/traité de la même durée ([`Fondu`]) ;
//! - **sécurité auditive** : 20 ms ne valent que pour une BAISSE de niveau.
//!   Toute bascule qui peut faire MONTER le niveau déclare d'abord sa hausse
//!   maximale ([`Traitement::hausse_si`]) ; l'étage TESS la confie à la borne
//!   de remontée de TESS ([`crate::hausses::PorteDesHausses`]), qui la retient
//!   et la rend en ~0,5 s sans jamais dépasser le niveau d'avant. Aucun
//!   mécanisme de remontée lente n'existe dans cette caisse.

use serde::{Deserialize, Serialize};

pub use tune_core::tess::etages_fixes::RAMPE_MS;

pub mod continu;
pub mod desaccentuation;
pub mod equilibre;
pub mod filtres;
pub mod loudness;
pub mod mono;
pub mod polarite;
pub mod reserve;
pub mod rumble;
pub mod ultrasonique;

#[cfg(test)]
pub(crate) mod essais;

/// Nombre maximal de canaux d'un flux traité (une trame sèche est gardée sur
/// la pile pendant un fondu).
pub const MAX_CANAUX: usize = 32;

/// Les traitements de la vague 1, dans l'ORDRE CANONIQUE de la chaîne : le
/// `Ord` dérivé suit l'ordre de déclaration, et la résolution d'un profil
/// trie sur lui.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenreTraitement {
    UltrasoniqueDsd,
    Desaccentuation,
    SuppressionContinu,
    Rumble,
    Polarite,
    Mono,
    Equilibre,
    Loudness,
    Reserve,
}

impl GenreTraitement {
    pub const TOUS: [GenreTraitement; 9] = [
        Self::UltrasoniqueDsd,
        Self::Desaccentuation,
        Self::SuppressionContinu,
        Self::Rumble,
        Self::Polarite,
        Self::Mono,
        Self::Equilibre,
        Self::Loudness,
        Self::Reserve,
    ];

    /// Identifiant stable (celui de l'étage TESS : `dsp-contextuel.<code>`).
    pub fn code(self) -> &'static str {
        match self {
            Self::UltrasoniqueDsd => "ultrasonique_dsd",
            Self::Desaccentuation => "desaccentuation",
            Self::SuppressionContinu => "suppression_continu",
            Self::Rumble => "rumble",
            Self::Polarite => "polarite",
            Self::Mono => "mono",
            Self::Equilibre => "equilibre",
            Self::Loudness => "loudness",
            Self::Reserve => "reserve",
        }
    }
}

/// Un canal d'un flux stéréo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Canal {
    Gauche,
    Droit,
}

impl Canal {
    pub fn indice(self) -> usize {
        match self {
            Self::Gauche => 0,
            Self::Droit => 1,
        }
    }
}

/// D'où vient un flux PCM, côté DSD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrigineDsd {
    /// Source PCM.
    #[default]
    Aucune,
    /// DSD joué en natif : jamais traité.
    Native,
    /// DSD encapsulé en DoP : jamais traité.
    Dop,
    /// DSD déjà converti en PCM par Tune : le seul cas où le filtre
    /// ultrasonique s'applique.
    ConvertieEnPcm,
}

/// Les réglages d'un traitement : une variante par genre.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "genre", rename_all = "snake_case")]
pub enum Reglages {
    UltrasoniqueDsd {
        coupure_hz: f64,
    },
    Desaccentuation,
    SuppressionContinu {
        coupure_hz: f64,
    },
    Rumble {
        coupure_hz: f64,
        voile: bool,
    },
    Polarite {
        canal: Canal,
    },
    Mono,
    /// `ecart_db` = niveau gauche − niveau droit mesuré (positif : gauche plus
    /// forte). Le traitement atténue le canal fort, au plus de
    /// [`equilibre::PLAFOND_DB`].
    Equilibre {
        ecart_db: f64,
    },
    Loudness {
        reference_phon: f64,
        palier: loudness::PalierLoudness,
    },
    /// Une CONTRAINTE exportée vers l'autorité de marge unique de TESS, pas un
    /// étage : [`Reglages::construire`] rend `None`. `crete_vraie_dbtp` :
    /// `None` tant qu'aucune mesure n'existe (aucune contrainte).
    Reserve {
        crete_vraie_dbtp: Option<f64>,
        marge_db: f64,
    },
}

impl Reglages {
    pub fn genre(&self) -> GenreTraitement {
        match self {
            Self::UltrasoniqueDsd { .. } => GenreTraitement::UltrasoniqueDsd,
            Self::Desaccentuation => GenreTraitement::Desaccentuation,
            Self::SuppressionContinu { .. } => GenreTraitement::SuppressionContinu,
            Self::Rumble { .. } => GenreTraitement::Rumble,
            Self::Polarite { .. } => GenreTraitement::Polarite,
            Self::Mono => GenreTraitement::Mono,
            Self::Equilibre { .. } => GenreTraitement::Equilibre,
            Self::Loudness { .. } => GenreTraitement::Loudness,
            Self::Reserve { .. } => GenreTraitement::Reserve,
        }
    }

    /// Les réglages par défaut d'un genre (choix « toujours » sans règle).
    pub fn par_defaut(genre: GenreTraitement) -> Self {
        match genre {
            GenreTraitement::UltrasoniqueDsd => Self::UltrasoniqueDsd {
                coupure_hz: ultrasonique::COUPURE_PAR_DEFAUT_HZ,
            },
            GenreTraitement::Desaccentuation => Self::Desaccentuation,
            GenreTraitement::SuppressionContinu => Self::SuppressionContinu {
                coupure_hz: continu::COUPURE_HZ,
            },
            GenreTraitement::Rumble => Self::Rumble {
                coupure_hz: rumble::COUPURE_PAR_DEFAUT_HZ,
                voile: false,
            },
            GenreTraitement::Polarite => Self::Polarite {
                canal: Canal::Droit,
            },
            GenreTraitement::Mono => Self::Mono,
            GenreTraitement::Equilibre => Self::Equilibre { ecart_db: 0.0 },
            GenreTraitement::Loudness => Self::Loudness {
                reference_phon: loudness::REFERENCE_PHON,
                palier: loudness::PalierLoudness::default(),
            },
            GenreTraitement::Reserve => Self::Reserve {
                crete_vraie_dbtp: None,
                marge_db: reserve::MARGE_PAR_DEFAUT_DB,
            },
        }
    }

    /// Construit le traitement, INACTIF (voir [`Traitement::activer`]).
    /// `origine` ne sert qu'au filtre ultrasonique. `None` pour la réserve,
    /// qui n'est pas un étage (voir [`reserve`]).
    pub fn construire(&self, origine: OrigineDsd) -> Option<Box<dyn Traitement>> {
        Some(match *self {
            Self::UltrasoniqueDsd { coupure_hz } => {
                Box::new(ultrasonique::Ultrasonique::new(coupure_hz, origine))
            }
            Self::Desaccentuation => Box::new(desaccentuation::Desaccentuation::new()),
            Self::SuppressionContinu { coupure_hz } => {
                Box::new(continu::SuppressionContinu::new(coupure_hz))
            }
            Self::Rumble { coupure_hz, voile } => Box::new(rumble::Rumble::new(coupure_hz, voile)),
            Self::Polarite { canal } => Box::new(polarite::Polarite::new(canal)),
            Self::Mono => Box::new(mono::Mono::new()),
            Self::Equilibre { ecart_db } => Box::new(equilibre::Equilibre::new(ecart_db)),
            Self::Loudness {
                reference_phon,
                palier,
            } => Box::new(loudness::Loudness::new(reference_phon, palier)),
            Self::Reserve { .. } => return None,
        })
    }
}

/// Pourquoi un traitement refuse un flux. `Copy`, sans allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErreurTraitement {
    /// Aucun canal, trop de canaux, ou cadence nulle.
    FormatNonPrisEnCharge { canaux: u16, cadence: u32 },
    /// Le filtre ultrasonique sur un flux qui n'est pas du DSD converti en PCM.
    OrigineNonPcm,
}

/// Un traitement de la vague 1.
pub trait Traitement: Send {
    fn genre(&self) -> GenreTraitement;

    /// HORS TEMPS RÉEL : alloue les états pour `canaux` à `cadence`. Remet le
    /// traitement au repos dans son état d'activation courant.
    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement>;

    /// TEMPS RÉEL : `trames` trames entrelacées, sur place, sans allocation.
    fn traiter(&mut self, tampon: &mut [f64], trames: usize);

    /// Vide les mémoires des filtres (saut, piste non gapless).
    fn reinitialiser(&mut self);

    /// Active ou coupe, par un fondu (ou une rampe de gain) de [`RAMPE_MS`].
    fn activer(&mut self, actif: bool);

    /// Termine sans attendre le fondu ou la rampe en cours (début de flux :
    /// rien ne jouait, il n'y a rien à fondre).
    fn sauter(&mut self);

    fn actif(&self) -> bool;

    /// Vrai quand [`Self::traiter`] ne touche AUCUN échantillon.
    fn est_neutre(&self) -> bool;

    /// La hausse de niveau MAXIMALE (dB, ≥ 0) qu'entraînerait le passage à
    /// `actif` depuis l'état visé actuel, sur la bande audible (20 Hz –
    /// min(20 kHz, 0,45·fs)), canal par canal. 0 : baisse ou niveau constant,
    /// la rampe de 20 ms suffit. Hors temps réel.
    fn hausse_si(&self, actif: bool) -> f64;

    /// HORS TEMPS RÉEL : volume de la zone, en dB (0 = plein volume). Seul le
    /// loudness s'en sert.
    fn definir_volume_db(&mut self, volume_db: f64) {
        let _ = volume_db;
    }
}

/// Longueur d'une rampe, en trames, à une cadence (même calcul que TESS).
pub fn trames_de_rampe(cadence: u32) -> f64 {
    (f64::from(cadence.max(1)) * f64::from(RAMPE_MS) / 1000.0).max(1.0)
}

/// dB → linéaire. Une valeur non finie vaut l'unité.
pub fn lineaire(db: f64) -> f64 {
    if db.is_finite() {
        10f64.powf(db / 20.0)
    } else {
        1.0
    }
}

/// Mélange sec/traité d'un traitement qui s'active ou se coupe : 0 = sec
/// (tampon intact), 1 = traité. Glisse linéairement en [`RAMPE_MS`].
#[derive(Debug, Clone, Copy)]
pub struct Fondu {
    courant: f64,
    cible: f64,
    pas: f64,
}

impl Default for Fondu {
    fn default() -> Self {
        Self {
            courant: 0.0,
            cible: 0.0,
            pas: 1.0,
        }
    }
}

impl Fondu {
    pub fn preparer(&mut self, cadence: u32) {
        self.pas = 1.0 / trames_de_rampe(cadence);
    }

    pub fn viser(&mut self, actif: bool) {
        self.cible = if actif { 1.0 } else { 0.0 };
    }

    pub fn sauter(&mut self) {
        self.courant = self.cible;
    }

    pub fn actif(&self) -> bool {
        self.cible == 1.0
    }

    pub fn eteint_au_repos(&self) -> bool {
        self.courant == 0.0 && self.cible == 0.0
    }

    pub fn allume_au_repos(&self) -> bool {
        self.courant == 1.0 && self.cible == 1.0
    }

    /// Le mélange de la trame courante, puis un pas vers la cible.
    fn avancer(&mut self) -> f64 {
        let m = self.courant;
        if self.courant < self.cible {
            self.courant = (self.courant + self.pas).min(self.cible);
        } else if self.courant > self.cible {
            self.courant = (self.courant - self.pas).max(self.cible);
        }
        m
    }

    /// Applique `calcul` (qui transforme une trame en sa version traitée) en
    /// respectant le fondu. Éteint au repos : le tampon n'est PAS touché et
    /// `calcul` n'est pas appelé. Rend vrai si `calcul` a tourné.
    pub fn appliquer<F: FnMut(&mut [f64])>(
        &mut self,
        tampon: &mut [f64],
        trames: usize,
        canaux: usize,
        mut calcul: F,
    ) -> bool {
        if self.eteint_au_repos() || canaux == 0 {
            return false;
        }
        let n = trames * canaux;
        if self.allume_au_repos() {
            tampon[..n].chunks_exact_mut(canaux).for_each(&mut calcul);
            return true;
        }
        let mut sec = [0.0f64; MAX_CANAUX];
        for trame in tampon[..n].chunks_exact_mut(canaux) {
            sec[..canaux].copy_from_slice(trame);
            calcul(trame);
            let m = self.avancer();
            for (y, x) in trame.iter_mut().zip(&sec[..canaux]) {
                *y = x + m * (*y - x);
            }
        }
        true
    }
}

/// Un gain qui glisse vers sa cible en [`RAMPE_MS`] (copie de la rampe de
/// TESS, `pub(crate)` dans `tune-core`). Au repos à l'unité, il ne touche
/// rien.
#[derive(Debug, Clone, Copy)]
pub struct RampeDeGain {
    courant: f64,
    cible: f64,
    pas: f64,
    trames_de_rampe: f64,
}

impl RampeDeGain {
    pub fn au_repos(gain: f64) -> Self {
        Self {
            courant: gain,
            cible: gain,
            pas: 0.0,
            trames_de_rampe: 1.0,
        }
    }

    pub fn preparer(&mut self, cadence: u32) {
        self.trames_de_rampe = trames_de_rampe(cadence);
    }

    pub fn viser(&mut self, cible: f64) {
        if cible != self.cible {
            self.cible = cible;
            self.pas = (cible - self.courant).abs() / self.trames_de_rampe;
        }
    }

    pub fn sauter(&mut self) {
        self.courant = self.cible;
        self.pas = 0.0;
    }

    pub fn cible(&self) -> f64 {
        self.cible
    }

    pub fn courant(&self) -> f64 {
        self.courant
    }

    pub fn unite_au_repos(&self) -> bool {
        self.courant == 1.0 && self.cible == 1.0
    }

    /// Le gain de la trame courante, puis un pas vers la cible.
    pub fn avancer(&mut self) -> f64 {
        let g = self.courant;
        if self.courant < self.cible {
            self.courant = (self.courant + self.pas).min(self.cible);
        } else if self.courant > self.cible {
            self.courant = (self.courant - self.pas).max(self.cible);
        }
        g
    }
}

/// Vérifie un format : au moins un canal, au plus [`MAX_CANAUX`], cadence non
/// nulle.
/// La plus forte hausse (dB, ≥ 0) que rend le passage de la réponse `avant`
/// à la réponse `apres` sur la bande audible : `max(0, apres(f) − avant(f))`.
/// Les écarts sous 1e-6 dB (arrondis) comptent pour zéro.
pub fn hausse_spectrale_db<A: Fn(f64) -> f64, B: Fn(f64) -> f64>(
    cadence: u32,
    avant: A,
    apres: B,
) -> f64 {
    let haut = 20_000f64.min(0.45 * f64::from(cadence.max(1)));
    let mut f = 20.0;
    let mut m = 0.0f64;
    while f <= haut {
        m = m.max(apres(f) - avant(f));
        f *= 1.02;
    }
    if m < 1e-6 { 0.0 } else { m }
}

pub(crate) fn verifier_format(canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
    if canaux == 0 || usize::from(canaux) > MAX_CANAUX || cadence == 0 {
        return Err(ErreurTraitement::FormatNonPrisEnCharge { canaux, cadence });
    }
    Ok(())
}
