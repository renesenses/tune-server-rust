//! L'étage fixe **Upsampling** de TESS (lot T11a), en `f64` de bout en bout.
//!
//! Place dans la chaîne (Q9) :
//!
//! ```text
//! Source → Headroom → greffons → Upsampling → Volume → Quantization → Output
//! ```
//!
//! Ce que l'étage change par rapport au suréchantillonneur de #7614
//! (`audio::surechantillonnage::EtageSurechantillonneur`, encore branché sur
//! la sortie locale tant que l'exécuteur de T03 n'est pas fusionné) :
//!
//! - **64 bits réels** : il prend et rend des `f64`, sans aucun passage par
//!   `f32` (l'ancien étage recevait et rendait des `f32`). Le calcul est celui
//!   de l'interpolateur polyphase du SDK (`Surechantillonneur`), inchangé.
//! - **Plus de réserve de 3 dB** : sous TESS, le headroom automatique de T07
//!   (−3 dB dès que l'upsampling est effectif) la remplace. L'étage ne touche
//!   donc pas au niveau (gain 1) ; seul reste le limiteur de sécurité du mode
//!   Réaliste (`tune_plugin_audio_support::limiteur`), **inerte au bit près**
//!   sous −0,2 dBFS, contre une crête intersample au-delà du headroom.
//! - **Changement de filtre à chaud** : [`EtageUpsampling::changer_de_filtre`]
//!   (hors temps réel, entre deux blocs) construit le nouveau filtre, l'amorce
//!   sur l'historique d'entrée qu'il garde (son état est alors celui qu'il
//!   aurait eu s'il avait tourné depuis le début), puis fond l'ancien dans le
//!   nouveau en [`RAMPE_MS`] (cosinus surélevé, somme des poids = 1). Le
//!   changement s'entend au bloc suivant : plus d'effet « à la piste
//!   suivante ».
//! - **Changement de cadence** (activer, désactiver, autre rapport ou autre
//!   plafond qui change la cadence retenue) : le périphérique doit être rouvert.
//!   [`planifier_changement`] le dit à l'hôte, qui relance TOUT DE SUITE à la
//!   position (mode « Immédiat » de T17, #8422), au lieu d'attendre la piste
//!   suivante.
//! - **Mise en forme du bruit** : l'étape finale est préparée sur le format de
//!   SORTIE de la chaîne (`preparer_chaine`), donc à la cadence suréchantillonnée :
//!   [`super::QuantificationMiseEnForme`] y choisit le formeur de cette cadence
//!   (`ordre2` au-delà de 48 kHz).
//! - **Réglages repris tels quels** (contrat B3, #7614) : filtre, `max_ratio`,
//!   `max_rate_hz` ([`ReglagesUpsamplingPcm`]), test de la machine compris
//!   ([`negocier`]).
//! - **Bit-perfect strict** (mode Pure) : l'étape reste refusée, avec le motif
//!   `pure_mode` de la résolution (aucune instance n'est construite).
//! - **Réseau** : la sortie locale seulement pour l'instant
//!   ([`transport_porte_l_upsampling`]) ; l'étage ne dépend d'aucun transport,
//!   OAAT le reçoit avec T04 (exécuteur réseau) puis T12a.
//!
//! Un changeur de cadence ne se contourne pas en cours de flux (sa sortie n'est
//! pas au format de son entrée) : [`EtageTess::definir_bypass`] est sans effet,
//! « Bypass all » et le décochage passent par la reconstruction de la chaîne.
//!
//! Règles temps réel : tout est alloué dans [`EtageTess::preparer`] et
//! [`EtageUpsampling::changer_de_filtre`] ; `traiter_vers`, `vidanger` et
//! `reinitialiser` n'allouent ni ne libèrent rien (l'ancien filtre, une fois
//! le fondu fini, est gardé jusqu'au prochain changement hors temps réel).

use serde_json::Value;
use tune_plugin_audio_support::limiteur::{CompteurDuLimiteur, Limiteur};
use tune_plugin_surechantillonnage::conception;
use tune_plugin_surechantillonnage::polyphase::Surechantillonneur;
use tune_plugin_surechantillonnage::sdk::{
    self as greffon, MAX_INTEGER_RATIO, OFFERED_FILTERS, UpsamplerSettings,
};

use super::rampe::RAMPE_MS;
use crate::audio::surechantillonnage::VerdictMachine;
use crate::tess::chaine::Upsampling;
use crate::tess::etage::{Disposition, ErreurEtage, EtageTess, FormatFlux, verifier_tampon};
use crate::tess::zones::TransportZone;

/// Identifiant de l'étage (code de l'étape `upsampling`).
pub const ID_UPSAMPLING: &str = "upsampling";

/// Pourquoi l'upsampling n'est pas monté. Le code est celui que le journal et
/// le chemin du signal (T16) affichent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotifUpsampling {
    /// Réglage lu `unknown` (chaîne d'un serveur plus récent).
    ValeurInconnue,
    /// Filtre, rapport ou plafond hors des valeurs du SDK.
    ReglagesInvalides,
    /// Upsampling vers DSD : le modulateur est le lot T11b.
    VersDsd,
    /// Aucun verdict du test de la machine (#7614, étape I5).
    TestDeLaMachineAbsent,
    /// La machine ne tient pas ce filtre aux cadences possibles.
    FiltreNonAdmis,
    /// Le périphérique n'accepte aucun multiple entier utile de la source
    /// sous les plafonds (relevés #8149 et #8259, « non appliqué »).
    AucuneCadence,
    /// Transport pas encore pris en charge (réseau : T04 puis T12).
    TransportEnAttente,
}

impl MotifUpsampling {
    pub fn code(self) -> &'static str {
        match self {
            Self::ValeurInconnue => "unknown_value",
            Self::ReglagesInvalides => "invalid_upsampling",
            Self::VersDsd => "dsd_modulator_pending",
            Self::TestDeLaMachineAbsent => "machine_test_missing",
            Self::FiltreNonAdmis => "filter_not_admitted",
            Self::AucuneCadence => "no_integer_rate_available",
            Self::TransportEnAttente => "transport_not_yet_supported",
        }
    }
}

/// Les réglages PCM de l'étage : exactement ceux d'`UpsamplerSettings`
/// (#7614), sans le drapeau `enabled` (sous TESS, c'est `mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReglagesUpsamplingPcm {
    /// Un des [`OFFERED_FILTERS`].
    pub filtre: &'static str,
    /// ×2, ×4, ×8 ou ×16 au plus ; `None` = maximum du périphérique.
    pub max_ratio: Option<u32>,
    /// Plafond de la cadence de sortie ; `None` = aucun.
    pub max_rate_hz: Option<u32>,
}

fn filtre_propose(id: &str) -> Option<&'static str> {
    OFFERED_FILTERS.iter().copied().find(|f| *f == id)
}

impl ReglagesUpsamplingPcm {
    /// Depuis `fixed.upsampling` de la chaîne : `Ok(None)` = éteint. La
    /// validation est celle du greffon (une seule source).
    pub fn depuis_chaine(u: &Upsampling) -> Result<Option<Self>, MotifUpsampling> {
        match u {
            Upsampling::Off {} => Ok(None),
            Upsampling::Unknown => Err(MotifUpsampling::ValeurInconnue),
            Upsampling::Dsd { .. } => Err(MotifUpsampling::VersDsd),
            Upsampling::Pcm {
                filter,
                max_ratio,
                max_rate_hz,
            } => {
                let r = UpsamplerSettings {
                    enabled: true,
                    filter: filter.clone(),
                    max_ratio: *max_ratio,
                    max_rate_hz: *max_rate_hz,
                };
                Self::depuis_greffon(&r)
                    .map(Some)
                    .ok_or(MotifUpsampling::ReglagesInvalides)
            }
        }
    }

    /// Depuis les réglages actuels de la zone (`zone_{id}_upsampler`) :
    /// `None` quand le greffon est désactivé ou le réglage invalide.
    pub fn depuis_greffon(s: &UpsamplerSettings) -> Option<Self> {
        if !s.enabled {
            return None;
        }
        let valide = greffon::settings(&serde_json::to_value(s).ok()?).ok()?;
        Some(Self {
            filtre: filtre_propose(&valide.filter)?,
            max_ratio: valide.max_ratio,
            max_rate_hz: valide.max_rate_hz,
        })
    }

    /// Vers les réglages du greffon (écriture croisée de T02, routes
    /// historiques, clients mobiles).
    pub fn vers_greffon(&self) -> UpsamplerSettings {
        UpsamplerSettings {
            enabled: true,
            filter: self.filtre.to_owned(),
            max_ratio: self.max_ratio,
            max_rate_hz: self.max_rate_hz,
        }
    }

    /// Vers `fixed.upsampling` de la chaîne.
    pub fn vers_chaine(&self) -> Upsampling {
        Upsampling::Pcm {
            filter: self.filtre.to_owned(),
            max_ratio: self.max_ratio,
            max_rate_hz: self.max_rate_hz,
        }
    }
}

/// La cadence de sortie pour une source à `source_hz`, parmi les cadences
/// que la sortie ACCEPTE (sondées par l'hôte) : la plus haute, multiple entier
/// de la source, sous les plafonds, que le test de la machine admet pour ce
/// filtre. Même règle que #7614 (`ChoixSurechantillonnage::negocier`).
pub fn negocier(
    reglages: &ReglagesUpsamplingPcm,
    source_hz: u32,
    acceptees: &[u32],
    verdict: Option<&VerdictMachine>,
) -> Result<u32, MotifUpsampling> {
    let s = reglages.vers_greffon();
    greffon::choose_rate_for(source_hz, acceptees, &s).ok_or(MotifUpsampling::AucuneCadence)?;
    let verdict = verdict.ok_or(MotifUpsampling::TestDeLaMachineAbsent)?;
    let admises: Vec<u32> = acceptees
        .iter()
        .copied()
        .filter(|&r| verdict.admet(reglages.filtre, r))
        .collect();
    greffon::choose_rate_for(source_hz, &admises, &s).ok_or(MotifUpsampling::FiltreNonAdmis)
}

/// Où l'étage peut tourner aujourd'hui. Il ne dépend d'aucun transport :
/// pour OAAT, T04 fournit l'exécuteur réseau et les cadences acceptées par
/// l'enceinte (`acceptees` de [`negocier`]) ; on lève alors la garde ici.
pub fn transport_porte_l_upsampling(t: TransportZone) -> Result<(), MotifUpsampling> {
    match t {
        TransportZone::Local => Ok(()),
        _ => Err(MotifUpsampling::TransportEnAttente),
    }
}

/// Ce que l'hôte doit faire quand la chaîne ou la zone change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangementUpsampling {
    /// Rien à faire.
    Aucun,
    /// Même cadence de sortie, autre filtre : [`EtageUpsampling::changer_de_filtre`]
    /// entre deux blocs, sans couper le son.
    FiltreAChaud { filtre: &'static str },
    /// La cadence de sortie change (activation, désactivation, rapport ou
    /// plafond) : relancer TOUT DE SUITE à la position (mode « Immédiat »),
    /// pas à la piste suivante.
    RouvrirLaSortie,
}

/// Décide du changement. `en_service` : les réglages et la cadence de sortie
/// OUVERTE ; `demande` : les nouveaux réglages et la cadence que [`negocier`]
/// rend pour eux (`None` : upsampling éteint ou écarté).
pub fn planifier_changement(
    en_service: Option<(&ReglagesUpsamplingPcm, u32)>,
    demande: Option<(&ReglagesUpsamplingPcm, u32)>,
) -> ChangementUpsampling {
    match (en_service, demande) {
        (None, None) => ChangementUpsampling::Aucun,
        (Some((a, cadence_a)), Some((b, cadence_b))) if cadence_a == cadence_b => {
            if a.filtre == b.filtre {
                ChangementUpsampling::Aucun
            } else {
                ChangementUpsampling::FiltreAChaud { filtre: b.filtre }
            }
        }
        _ => ChangementUpsampling::RouvrirLaSortie,
    }
}

/// Un interpolateur et ce qu'on en sait.
struct Moteur {
    interpolateur: Surechantillonneur,
    filtre: &'static str,
    /// Position du pic du noyau, en trames de sortie (comme le greffon).
    latence: u32,
}

impl Moteur {
    fn nouveau(filtre: &str, de: u32, vers: u32, canaux: usize) -> Result<Self, ErreurEtage> {
        let filtre = filtre_propose(filtre).ok_or(ErreurEtage::Reglages("filtre inconnu"))?;
        let c = conception::par_id(filtre).ok_or(ErreurEtage::Reglages("filtre inconnu"))?;
        let interpolateur = Surechantillonneur::nouveau(&c, de, vers, canaux).map_err(|_| {
            ErreurEtage::FormatNonPrisEnCharge {
                channels: canaux as u16,
                sample_rate_hz: de,
            }
        })?;
        let pic = interpolateur
            .noyau_etage1()
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map_or(0, |(i, _)| i);
        Ok(Self {
            interpolateur,
            filtre,
            latence: u32::try_from(pic).unwrap_or(u32::MAX),
        })
    }
}

/// La plus longue portée (en trames d'entrée) des filtres proposés : la
/// longueur d'historique qui suffit à amorcer n'importe lequel.
fn portee_max() -> usize {
    OFFERED_FILTERS
        .iter()
        .filter_map(|f| conception::par_id(f))
        .map(|c| c.portee)
        .max()
        .unwrap_or(0)
}

/// L'étage. Voir le module.
pub struct EtageUpsampling {
    filtre: &'static str,
    cadence_sortie: u32,
    entree: Option<FormatFlux>,
    canaux: usize,
    rapport: usize,
    max_trames: usize,
    moteur: Option<Moteur>,
    /// Le filtre vers lequel on fond, amorcé.
    suivant: Option<Moteur>,
    /// L'ancien filtre, une fois le fondu fini : libéré hors temps réel.
    remplace: Option<Moteur>,
    fondu_trames: usize,
    fondu_position: usize,
    entree_propre: Vec<f64>,
    sortie_moteur: Vec<f64>,
    sortie_suivant: Vec<f64>,
    /// Anneau des dernières trames d'entrée (`canaux × longueur`).
    historique: Vec<f64>,
    historique_trames: usize,
    historique_w: usize,
    limiteur: Limiteur,
    zeros: Vec<f64>,
    queue_restante: u64,
    en_attente: usize,
    decalage_attente: usize,
}

impl EtageUpsampling {
    /// Un étage pour `filtre` vers `cadence_sortie` (rendue par [`negocier`],
    /// cadence à laquelle la sortie est ouverte).
    pub fn nouveau(filtre: &str, cadence_sortie: u32) -> Result<Self, ErreurEtage> {
        let filtre = filtre_propose(filtre).ok_or(ErreurEtage::Reglages("filtre inconnu"))?;
        Ok(Self {
            filtre,
            cadence_sortie,
            entree: None,
            canaux: 0,
            rapport: 0,
            max_trames: 0,
            moteur: None,
            suivant: None,
            remplace: None,
            fondu_trames: 1,
            fondu_position: 0,
            entree_propre: Vec::new(),
            sortie_moteur: Vec::new(),
            sortie_suivant: Vec::new(),
            historique: Vec::new(),
            historique_trames: 0,
            historique_w: 0,
            limiteur: Limiteur::new(cadence_sortie),
            zeros: Vec::new(),
            queue_restante: 0,
            en_attente: 0,
            decalage_attente: 0,
        })
    }

    /// Le filtre en service (celui vers lequel on fond, pendant un fondu).
    pub fn filtre(&self) -> &'static str {
        self.filtre
    }

    pub fn cadence_sortie(&self) -> u32 {
        self.cadence_sortie
    }

    /// Latence du filtre qui joue, en trames de sortie.
    pub fn latence_trames(&self) -> u32 {
        self.moteur.as_ref().map_or(0, |m| m.latence)
    }

    pub fn fondu_en_cours(&self) -> bool {
        self.suivant.is_some()
    }

    /// Ce que le limiteur de sécurité a fait depuis l'ouverture.
    pub fn compteur_du_limiteur(&self) -> CompteurDuLimiteur {
        self.limiteur.compteur()
    }

    /// HORS TEMPS RÉEL, entre deux blocs. Passe à `filtre` sans couper le son
    /// ni changer de cadence. Voir le module.
    pub fn changer_de_filtre(&mut self, filtre: &str) -> Result<(), ErreurEtage> {
        let filtre = filtre_propose(filtre).ok_or(ErreurEtage::Reglages("filtre inconnu"))?;
        // L'ancien filtre d'un fondu fini est libéré ici, hors temps réel.
        self.remplace = None;
        let Some(entree) = self.entree else {
            self.filtre = filtre;
            return Ok(());
        };
        let en_service = self.moteur.as_ref().map(|m| m.filtre);
        if en_service == Some(filtre) {
            // Retour au filtre qui joue : on abandonne le fondu en cours.
            self.suivant = None;
            self.fondu_position = 0;
            self.filtre = filtre;
            return Ok(());
        }
        let mut m = Moteur::nouveau(
            filtre,
            entree.sample_rate_hz,
            self.cadence_sortie,
            self.canaux,
        )?;
        // Amorce : l'historique, du plus ancien au plus récent. L'état du
        // nouvel interpolateur est alors celui qu'il aurait eu en jouant tout
        // le flux (fenêtre identique, même phase en rapport entier).
        let ch = self.canaux;
        let n = self.historique_trames;
        let mut amorce = Vec::with_capacity(n * ch);
        amorce.extend_from_slice(&self.historique[self.historique_w * ch..n * ch]);
        amorce.extend_from_slice(&self.historique[..self.historique_w * ch]);
        let mut jete = Vec::with_capacity(n * self.rapport * ch);
        m.interpolateur.traiter(&amorce, &mut jete);
        self.suivant = Some(m);
        self.fondu_position = 0;
        self.filtre = filtre;
        Ok(())
    }

    fn noter_historique(&mut self, entree: &[f64]) {
        let ch = self.canaux;
        let n = self.historique_trames;
        if n == 0 {
            return;
        }
        for trame in entree.chunks_exact(ch) {
            let w = self.historique_w;
            self.historique[w * ch..w * ch + ch].copy_from_slice(trame);
            self.historique_w = if w + 1 == n { 0 } else { w + 1 };
        }
    }

    /// Fin du fondu : le suivant joue, l'ancien attend d'être libéré hors
    /// temps réel. Ni allocation ni libération.
    fn achever_le_fondu(&mut self) {
        if let Some(suivant) = self.suivant.take() {
            debug_assert!(self.remplace.is_none());
            self.remplace = self.moteur.replace(suivant);
            std::mem::swap(&mut self.sortie_moteur, &mut self.sortie_suivant);
            self.fondu_position = 0;
            self.queue_restante = u64::from(self.latence_trames());
        }
    }

    fn limiter(&mut self, sortie: &mut [f64]) {
        let ch = self.canaux;
        for trame in sortie.chunks_exact_mut(ch) {
            let crete = trame.iter().fold(0.0f64, |m, x| m.max(x.abs()));
            let g = self.limiteur.gain(crete);
            if g != 1.0 {
                trame.iter_mut().for_each(|x| *x *= g);
            }
        }
    }
}

impl EtageTess for EtageUpsampling {
    fn id(&self) -> &str {
        ID_UPSAMPLING
    }

    fn preparer(
        &mut self,
        entree: FormatFlux,
        max_trames: usize,
    ) -> Result<Disposition, ErreurEtage> {
        let refus = ErreurEtage::FormatNonPrisEnCharge {
            channels: entree.channels,
            sample_rate_hz: entree.sample_rate_hz,
        };
        let de = entree.sample_rate_hz;
        if entree.channels == 0 || de == 0 || !self.cadence_sortie.is_multiple_of(de) {
            return Err(refus);
        }
        let rapport = (self.cadence_sortie / de) as usize;
        if !(2..=MAX_INTEGER_RATIO as usize).contains(&rapport) {
            return Err(refus);
        }
        let ch = usize::from(entree.channels);
        let moteur = Moteur::nouveau(self.filtre, de, self.cadence_sortie, ch)?;
        let max_sortie = max_trames * rapport;
        self.canaux = ch;
        self.rapport = rapport;
        self.max_trames = max_trames;
        self.queue_restante = u64::from(moteur.latence);
        self.moteur = Some(moteur);
        self.suivant = None;
        self.remplace = None;
        self.fondu_trames = (self.cadence_sortie as usize * RAMPE_MS as usize / 1000).max(1);
        self.fondu_position = 0;
        self.entree_propre = vec![0.0; max_trames * ch];
        self.sortie_moteur = Vec::with_capacity(max_sortie * ch);
        self.sortie_suivant = Vec::with_capacity(max_sortie * ch);
        self.historique_trames = portee_max();
        self.historique = vec![0.0; self.historique_trames * ch];
        self.historique_w = 0;
        self.limiteur = Limiteur::new(self.cadence_sortie);
        self.zeros = vec![0.0; max_trames * ch];
        self.en_attente = 0;
        self.decalage_attente = 0;
        self.entree = Some(entree);
        Ok(Disposition {
            sortie: FormatFlux {
                channels: entree.channels,
                sample_rate_hz: self.cadence_sortie,
            },
            max_trames_sortie: max_sortie,
            latence_trames: self.latence_trames(),
            sur_place: false,
        })
    }

    fn traiter_vers(
        &mut self,
        entree: &[f64],
        trames: usize,
        sortie: &mut [f64],
    ) -> Result<usize, ErreurEtage> {
        let format = self.entree.ok_or(ErreurEtage::NonPrepare)?;
        if trames > self.max_trames {
            return Err(ErreurEtage::BlocTropGrand {
                trames,
                max: self.max_trames,
            });
        }
        let ch = self.canaux;
        let ecrites = trames * self.rapport;
        verifier_tampon(entree.len(), trames, format.channels)?;
        verifier_tampon(sortie.len(), ecrites, format.channels)?;
        let n = trames * ch;
        // Un non-fini ne doit pas empoisonner l'historique du filtre.
        for (d, s) in self.entree_propre[..n].iter_mut().zip(&entree[..n]) {
            *d = if s.is_finite() { *s } else { 0.0 };
        }
        let propre = std::mem::take(&mut self.entree_propre);
        self.noter_historique(&propre[..n]);
        let Some(moteur) = self.moteur.as_mut() else {
            self.entree_propre = propre;
            return Err(ErreurEtage::NonPrepare);
        };
        self.sortie_moteur.clear();
        moteur
            .interpolateur
            .traiter(&propre[..n], &mut self.sortie_moteur);
        let m = ecrites * ch;
        if let Some(suivant) = self.suivant.as_mut() {
            self.sortie_suivant.clear();
            suivant
                .interpolateur
                .traiter(&propre[..n], &mut self.sortie_suivant);
            let total = self.fondu_trames as f64;
            for t in 0..ecrites {
                let k = self.fondu_position;
                let a = &self.sortie_moteur[t * ch..t * ch + ch];
                let b = &self.sortie_suivant[t * ch..t * ch + ch];
                let d = &mut sortie[t * ch..t * ch + ch];
                if k < self.fondu_trames {
                    let w = 0.5 - 0.5 * (std::f64::consts::PI * k as f64 / total).cos();
                    for ((d, a), b) in d.iter_mut().zip(a).zip(b) {
                        *d = a * (1.0 - w) + b * w;
                    }
                    self.fondu_position += 1;
                } else {
                    d.copy_from_slice(b);
                }
            }
            if self.fondu_position >= self.fondu_trames {
                self.achever_le_fondu();
            }
        } else {
            sortie[..m].copy_from_slice(&self.sortie_moteur[..m]);
        }
        self.entree_propre = propre;
        self.limiter(&mut sortie[..m]);
        Ok(ecrites)
    }

    fn vidanger(&mut self, sortie: &mut [f64]) -> Result<usize, ErreurEtage> {
        self.entree.ok_or(ErreurEtage::NonPrepare)?;
        self.achever_le_fondu();
        let ch = self.canaux;
        let capacite = sortie.len() / ch.max(1);
        if capacite == 0 {
            return Err(ErreurEtage::TamponTropPetit {
                requis: ch,
                capacite: sortie.len(),
            });
        }
        if self.en_attente == 0 && self.queue_restante > 0 {
            let voulu = self.queue_restante.min(capacite as u64) as usize;
            let a_nourrir = voulu.div_ceil(self.rapport).min(self.max_trames);
            let Some(moteur) = self.moteur.as_mut() else {
                return Err(ErreurEtage::NonPrepare);
            };
            self.sortie_moteur.clear();
            moteur
                .interpolateur
                .traiter(&self.zeros[..a_nourrir * ch], &mut self.sortie_moteur);
            self.en_attente = (self.sortie_moteur.len() / ch).min(self.queue_restante as usize);
            self.decalage_attente = 0;
        }
        let n = self.en_attente.min(capacite);
        let de = self.decalage_attente * ch;
        sortie[..n * ch].copy_from_slice(&self.sortie_moteur[de..de + n * ch]);
        self.limiter(&mut sortie[..n * ch]);
        self.en_attente -= n;
        self.decalage_attente += n;
        self.queue_restante -= n as u64;
        Ok(n)
    }

    fn reinitialiser(&mut self) {
        self.achever_le_fondu();
        if let Some(m) = self.moteur.as_mut() {
            m.interpolateur.reinitialiser();
            self.queue_restante = u64::from(m.latence);
        }
        self.historique.iter_mut().for_each(|x| *x = 0.0);
        self.historique_w = 0;
        self.limiteur.oublier();
        self.en_attente = 0;
        self.decalage_attente = 0;
    }

    /// Un changeur de cadence n'est jamais contourné en cours de flux.
    fn bypass(&self) -> bool {
        false
    }

    /// Sans effet : voir le module (reconstruction de la chaîne).
    fn definir_bypass(&mut self, _bypass: bool) {}

    /// HORS TEMPS RÉEL. `reglages` : la valeur de `fixed.upsampling`. Un
    /// autre filtre à la même cadence passe à chaud ; tout ce qui change la
    /// cadence (ou l'éteint) demande la reconstruction ([`planifier_changement`]).
    fn appliquer_reglages(&mut self, reglages: &Value) -> Result<(), ErreurEtage> {
        let u: Upsampling = serde_json::from_value(reglages.clone())
            .map_err(|_| ErreurEtage::Reglages("invalid_upsampling"))?;
        let r = match ReglagesUpsamplingPcm::depuis_chaine(&u) {
            Ok(Some(r)) => r,
            Ok(None) | Err(MotifUpsampling::VersDsd) | Err(MotifUpsampling::ValeurInconnue) => {
                return Err(ErreurEtage::ReconstructionRequise);
            }
            Err(_) => return Err(ErreurEtage::Reglages("invalid_upsampling")),
        };
        if let Some(entree) = self.entree {
            let garde = greffon::choose_rate_for(
                entree.sample_rate_hz,
                &[self.cadence_sortie],
                &r.vers_greffon(),
            );
            if garde != Some(self.cadence_sortie) {
                return Err(ErreurEtage::ReconstructionRequise);
            }
        }
        self.changer_de_filtre(r.filtre)
    }
}

#[cfg(test)]
#[path = "upsampling_tests.rs"]
mod tests;
