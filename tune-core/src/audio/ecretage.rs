//! #2218 (suite de T9) — l'écrêtage est COMPTÉ et DIT, sans changer un
//! échantillon.
//!
//! T9 (`docs/mesures/2218-marge-ecretage-crete-vraie.md`) a mesuré que
//! `replaygain::apply_gain_pcm` écrête dur 66 % d'un sinus à −0,1 dBFS sous
//! +6 dB **sans compteur ni journal**, et que l'égaliseur compte ses `overs`
//! (83,7 % sur un passe-bas Q = 4) **sans jamais les journaliser**. Ce module
//! n'ajoute que des compteurs, des lignes de journal et une exposition : le
//! clamp de chaque étage reste où il est, dans l'ordre où il est (clamp PUIS
//! dither pour l'égaliseur), avec ses seuils. Les défauts A–E de T9 (pas de
//! garde sans pic tagué, réserve qui ignore Q, crête vraie, 24→16 sans dither,
//! troncature vers zéro) sont des décisions de conception à trancher par
//! Bertrand ; ici on les rend visibles, on ne les corrige pas.
//!
//! Trois niveaux :
//! * [`CompteurDEcretage`] — champs simples, **zéro allocation**, incrémenté là
//!   où le clamp a lieu : par piste pour l'égaliseur (`EqProcessor::ecretage`)
//!   et pour [`GainReplay`](crate::audio::replaygain::GainReplay) ; par appel
//!   pour `apply_gain_pcm` (qui ne connaît pas la piste) et le mixeur ;
//! * [`dire_premier`] / [`dire_fin`] — la ligne de journal **`dsp_ecretage`**,
//!   UNE fois au premier écrêtage de la piste et UNE fois à sa fin avec le
//!   total, jamais par bloc : un `warn!` par bloc à 44 100 Hz noierait le
//!   journal ;
//! * [`REGISTRE`] — les totaux par étage depuis le démarrage du processus,
//!   en `AtomicU64`, lus par le rapport de diagnostic (`dsp_ecretage`).
//!
//! **Fil d'exécution.** Ces étages tournent côté PRODUCTEUR, jamais dans un
//! rappel temps réel : le bras progressif les applique dans une tâche tokio
//! (`spawn_streaming_dsp_relay`, `orchestrator.rs`), le transcodage complet
//! dans sa propre tâche (`orchestrator/transcodage.rs`), et la sortie locale
//! dans `apply_local_dsp`, appelé par `process_pcm_chunk` /
//! `prepare_windows_*_pcm` / `play_url` — les rappels cpal
//! (`build_output_stream`) ne font que vider l'anneau. Un `warn!` émis après
//! un bloc, dans ces chemins, n'est donc pas un `warn!` dans un rappel audio.
//!
//! **Zone.** Aucun de ces étages ne connaît sa zone : `apply_gain_pcm` reçoit
//! un facteur, `EqProcessor::new` un profil. La ligne porte l'étage et les
//! compteurs ; la zone vient du contexte (`tracing::Span`) quand l'appelant
//! en tient un, et le rapport de diagnostic cumule par étage.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tracing::warn;

/// L'étage qui écrête.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtageEcretant {
    /// `replaygain::apply_gain_pcm` — clamp puis `as i16` / `as i32`.
    ReplayGain,
    /// `eq::EqProcessor` — `write_sample_f64`, clamp à 1,0 − 1 LSB puis dither.
    Egaliseur,
    /// `mixer::PcmMixer` — `SampleFormat::write` et `mix_buffers`.
    Mixeur,
}

impl EtageEcretant {
    /// Le nom qui part dans le journal et dans le rapport.
    pub fn nom(self) -> &'static str {
        match self {
            Self::ReplayGain => "replaygain",
            Self::Egaliseur => "egaliseur",
            Self::Mixeur => "mixeur",
        }
    }
}

/// Portée d'une ligne de journal : une piste (l'étage connaît son début et sa
/// fin) ou le processus (l'étage ne reçoit que des blocs, sans piste).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Portee {
    Piste,
    Processus,
}

impl Portee {
    fn nom(self) -> &'static str {
        match self {
            Self::Piste => "piste",
            Self::Processus => "processus",
        }
    }
}

/// Compteur d'écrêtage d'un étage : champs simples, zéro allocation,
/// incrémenté là où le clamp a lieu SANS toucher au clamp.
///
/// « Écrêté » = l'échantillon idéal (entrée × facteur, ou sortie de la
/// cascade) est au-delà du seuil que l'étage sature — exactement la condition
/// du clamp de l'étage, pas une définition parallèle. L'excès est mesuré en
/// LSB de la profondeur traitée (pour le chemin flottant de l'égaliseur, qui
/// n'a pas de profondeur, la référence est 24 bits) ; `crete_max` est la
/// crête idéale relative à la pleine échelle (1,0 = le rail), d'où le rapport
/// tire les dBFS.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CompteurDEcretage {
    /// Échantillons passés par l'étage (tous canaux confondus).
    pub echantillons_vus: u64,
    /// Échantillons que le clamp a ramenés au rail.
    pub echantillons_ecretes: u64,
    /// Excès maximal au-delà du rail, en LSB, arrondi.
    pub exces_max_lsb: u64,
    /// Crête idéale maximale, relative à la pleine échelle (≥ 1,0 dès qu'un
    /// échantillon est écrêté ; 0,0 tant que rien ne l'est).
    pub crete_max: f64,
    /// Index (en échantillons depuis le début du comptage) du premier
    /// échantillon écrêté.
    pub premier_ecretage_a: Option<u64>,
}

impl CompteurDEcretage {
    /// Un échantillon écrêté, à `index`, dépassant le rail de `exces_lsb` LSB
    /// et culminant à `crete` (relatif à la pleine échelle).
    #[inline]
    pub fn noter_ecrete(&mut self, index: u64, exces_lsb: f64, crete: f64) {
        if self.echantillons_ecretes == 0 {
            self.premier_ecretage_a = Some(index);
        }
        self.echantillons_ecretes += 1;
        let e = exces_lsb.max(0.0).round() as u64;
        if e > self.exces_max_lsb {
            self.exces_max_lsb = e;
        }
        if crete > self.crete_max {
            self.crete_max = crete;
        }
    }

    /// `n` échantillons passés par l'étage (une fois par bloc, pas par
    /// échantillon : le chemin d'échantillons ne paie qu'une addition).
    #[inline]
    pub fn noter_vus(&mut self, n: u64) {
        self.echantillons_vus += n;
    }

    /// Pourcentage d'échantillons écrêtés, 0,0 sans échantillon.
    pub fn pourcentage(&self) -> f64 {
        if self.echantillons_vus == 0 {
            0.0
        } else {
            100.0 * self.echantillons_ecretes as f64 / self.echantillons_vus as f64
        }
    }

    /// Crête idéale maximale en dBFS (0,0 = le rail), `None` sans écrêtage.
    pub fn crete_max_dbfs(&self) -> Option<f64> {
        (self.crete_max > 0.0).then(|| 20.0 * self.crete_max.log10())
    }

    /// Fusionne `autre` (une suite de blocs) dans `self` : sert au relais
    /// d'un processeur remplacé à chaud et au registre.
    pub fn cumuler(&mut self, autre: &CompteurDEcretage) {
        if self.echantillons_ecretes == 0
            && let Some(p) = autre.premier_ecretage_a
        {
            self.premier_ecretage_a = Some(self.echantillons_vus + p);
        }
        self.echantillons_vus += autre.echantillons_vus;
        self.echantillons_ecretes += autre.echantillons_ecretes;
        self.exces_max_lsb = self.exces_max_lsb.max(autre.exces_max_lsb);
        if autre.crete_max > self.crete_max {
            self.crete_max = autre.crete_max;
        }
    }
}

/// UNE ligne au premier écrêtage — celle qui dit « ça écrête » pendant que la
/// piste joue encore, avec où et de combien.
pub fn dire_premier(etage: EtageEcretant, portee: Portee, c: &CompteurDEcretage) {
    REGISTRE
        .etage(etage)
        .lignes_journal
        .fetch_add(1, Ordering::Relaxed);
    warn!(
        etage = etage.nom(),
        moment = "premier",
        portee = portee.nom(),
        echantillons_vus = c.echantillons_vus,
        echantillons_ecretes = c.echantillons_ecretes,
        pourcentage = format_args!("{:.1}", c.pourcentage()),
        exces_max_lsb = c.exces_max_lsb,
        crete_max_dbfs = format_args!("{:+.2}", c.crete_max_dbfs().unwrap_or(0.0)),
        premier_ecretage_a = c.premier_ecretage_a.unwrap_or(0),
        "dsp_ecretage"
    );
}

/// UNE ligne en fin de piste, avec le total. Silencieuse si rien n'a été
/// écrêté : une piste propre ne laisse pas de trace.
pub fn dire_fin(etage: EtageEcretant, portee: Portee, c: &CompteurDEcretage) {
    if c.echantillons_ecretes == 0 {
        return;
    }
    REGISTRE
        .etage(etage)
        .lignes_journal
        .fetch_add(1, Ordering::Relaxed);
    warn!(
        etage = etage.nom(),
        moment = "fin",
        portee = portee.nom(),
        echantillons_vus = c.echantillons_vus,
        echantillons_ecretes = c.echantillons_ecretes,
        pourcentage = format_args!("{:.1}", c.pourcentage()),
        exces_max_lsb = c.exces_max_lsb,
        crete_max_dbfs = format_args!("{:+.2}", c.crete_max_dbfs().unwrap_or(0.0)),
        premier_ecretage_a = c.premier_ecretage_a.unwrap_or(0),
        "dsp_ecretage"
    );
}

/// Totaux d'un étage depuis le démarrage du processus. Des atomiques, pas de
/// verrou : le chemin d'échantillons n'attend jamais derrière le rapport.
pub struct TotauxEtage {
    echantillons_vus: AtomicU64,
    echantillons_ecretes: AtomicU64,
    exces_max_lsb: AtomicU64,
    /// Appels (blocs ou pistes) qui ont écrêté au moins un échantillon.
    appels_ecretants: AtomicU64,
    /// Pistes closes (`dire_fin`) avec au moins un échantillon écrêté.
    pistes_ecretees: AtomicU64,
    /// Lignes `dsp_ecretage` émises pour cet étage.
    lignes_journal: AtomicU64,
}

impl TotauxEtage {
    const fn new() -> Self {
        Self {
            echantillons_vus: AtomicU64::new(0),
            echantillons_ecretes: AtomicU64::new(0),
            exces_max_lsb: AtomicU64::new(0),
            appels_ecretants: AtomicU64::new(0),
            pistes_ecretees: AtomicU64::new(0),
            lignes_journal: AtomicU64::new(0),
        }
    }

    /// Ajoute le DELTA d'un bloc (`apres` − `avant` d'un compteur de piste,
    /// ou un compteur d'appel avec `avant` à zéro). Rend `true` quand ce bloc
    /// est le PREMIER du processus à écrêter sur cet étage — ce qui permet à un
    /// étage sans notion de piste de le dire une fois, et une seule.
    pub fn absorber(&self, avant: &CompteurDEcretage, apres: &CompteurDEcretage) -> bool {
        let vus = apres
            .echantillons_vus
            .saturating_sub(avant.echantillons_vus);
        let ecretes = apres
            .echantillons_ecretes
            .saturating_sub(avant.echantillons_ecretes);
        self.echantillons_vus.fetch_add(vus, Ordering::Relaxed);
        if ecretes == 0 {
            return false;
        }
        let d_avant = self
            .echantillons_ecretes
            .fetch_add(ecretes, Ordering::Relaxed);
        self.appels_ecretants.fetch_add(1, Ordering::Relaxed);
        self.exces_max_lsb
            .fetch_max(apres.exces_max_lsb, Ordering::Relaxed);
        d_avant == 0
    }

    /// Une piste close avec écrêtage.
    pub fn piste_close(&self) {
        self.pistes_ecretees.fetch_add(1, Ordering::Relaxed);
    }

    /// Photographie lisible.
    pub fn releve(&self) -> ReleveEtage {
        let vus = self.echantillons_vus.load(Ordering::Relaxed);
        let ecretes = self.echantillons_ecretes.load(Ordering::Relaxed);
        ReleveEtage {
            echantillons_vus: vus,
            echantillons_ecretes: ecretes,
            pourcentage: if vus == 0 {
                0.0
            } else {
                (1000.0 * ecretes as f64 / vus as f64).round() / 10.0
            },
            exces_max_lsb: self.exces_max_lsb.load(Ordering::Relaxed),
            appels_ecretants: self.appels_ecretants.load(Ordering::Relaxed),
            pistes_ecretees: self.pistes_ecretees.load(Ordering::Relaxed),
            lignes_journal: self.lignes_journal.load(Ordering::Relaxed),
        }
    }
}

/// Les totaux des trois étages, depuis le démarrage.
pub struct Registre {
    pub replaygain: TotauxEtage,
    pub egaliseur: TotauxEtage,
    pub mixeur: TotauxEtage,
}

impl Registre {
    /// Les totaux d'un étage.
    pub fn etage(&self, e: EtageEcretant) -> &TotauxEtage {
        match e {
            EtageEcretant::ReplayGain => &self.replaygain,
            EtageEcretant::Egaliseur => &self.egaliseur,
            EtageEcretant::Mixeur => &self.mixeur,
        }
    }
}

/// Le registre du processus.
pub static REGISTRE: Registre = Registre {
    replaygain: TotauxEtage::new(),
    egaliseur: TotauxEtage::new(),
    mixeur: TotauxEtage::new(),
};

/// Un étage dans le rapport de diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReleveEtage {
    pub echantillons_vus: u64,
    pub echantillons_ecretes: u64,
    /// Arrondi au dixième.
    pub pourcentage: f64,
    pub exces_max_lsb: u64,
    pub appels_ecretants: u64,
    pub pistes_ecretees: u64,
    pub lignes_journal: u64,
}

/// La section `dsp_ecretage` du rapport de diagnostic : ce que chaque étage a
/// écrêté depuis le démarrage, compté là où le clamp a lieu.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReleveEcretage {
    pub replaygain: ReleveEtage,
    pub egaliseur: ReleveEtage,
    pub mixeur: ReleveEtage,
}

impl ReleveEcretage {
    /// Les trois étages, nommés, dans l'ordre du bras.
    pub fn etages(&self) -> [(&'static str, &ReleveEtage); 3] {
        [
            ("replaygain", &self.replaygain),
            ("egaliseur", &self.egaliseur),
            ("mixeur", &self.mixeur),
        ]
    }
}

/// Photographie du registre, pour le rapport.
pub fn releve() -> ReleveEcretage {
    ReleveEcretage {
        replaygain: REGISTRE.replaygain.releve(),
        egaliseur: REGISTRE.egaliseur.releve(),
        mixeur: REGISTRE.mixeur.releve(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_compteur_vide_ne_dit_rien_et_vaut_zero() {
        let c = CompteurDEcretage::default();
        assert_eq!(c.pourcentage(), 0.0);
        assert_eq!(c.crete_max_dbfs(), None);
        assert_eq!(c.premier_ecretage_a, None);
    }

    #[test]
    fn le_premier_ecretage_garde_son_index_et_l_exces_son_maximum() {
        let mut c = CompteurDEcretage::default();
        c.noter_vus(10);
        c.noter_ecrete(12, 3.4, 1.1);
        c.noter_ecrete(15, 31_866.0, 1.97);
        c.noter_ecrete(16, 0.2, 1.0);
        c.noter_vus(10);
        assert_eq!(c.premier_ecretage_a, Some(12));
        assert_eq!(c.echantillons_ecretes, 3);
        assert_eq!(c.exces_max_lsb, 31_866);
        assert_eq!(c.echantillons_vus, 20);
        assert_eq!(c.pourcentage(), 15.0);
        assert!((c.crete_max_dbfs().unwrap() - 5.89).abs() < 0.01);
    }

    #[test]
    fn cumuler_decale_l_index_du_premier_ecretage() {
        let mut a = CompteurDEcretage::default();
        a.noter_vus(100);
        let mut b = CompteurDEcretage::default();
        b.noter_ecrete(7, 2.0, 1.01);
        b.noter_vus(50);
        a.cumuler(&b);
        assert_eq!(a.premier_ecretage_a, Some(107));
        assert_eq!(a.echantillons_vus, 150);
        assert_eq!(a.echantillons_ecretes, 1);
    }

    #[test]
    fn le_registre_ne_signale_le_premier_bloc_du_processus_qu_une_fois() {
        let t = TotauxEtage::new();
        let zero = CompteurDEcretage::default();
        let mut c = CompteurDEcretage::default();
        c.noter_vus(4);
        assert!(!t.absorber(&zero, &c), "sans écrêtage : rien à signaler");
        c.noter_ecrete(1, 5.0, 1.2);
        assert!(t.absorber(&zero, &c), "premier bloc écrêtant du processus");
        assert!(!t.absorber(&zero, &c), "le second ne l'est plus");
        let r = t.releve();
        assert_eq!(r.echantillons_ecretes, 2);
        assert_eq!(r.appels_ecretants, 2);
        assert_eq!(r.exces_max_lsb, 5);
        assert_eq!(r.echantillons_vus, 12);
        assert!((r.pourcentage - 16.7).abs() < 1e-9);
    }
}
