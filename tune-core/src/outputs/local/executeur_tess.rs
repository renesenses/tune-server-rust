//! TESS T03 — la chaîne DSP de la sortie locale passe par l'exécuteur TESS,
//! en `f64`.
//!
//! Ce module remplace le corps de l'ancien `apply_local_dsp` (chaîne `f32`
//! câblée en dur : égaliseur, convolveur, crossfeed, repli mono).
//!
//! - **Exécuteur** : [`crate::tess::Executeur`], un par fil (`thread_local`),
//!   réutilisé d'un bloc à l'autre : ses préparations tiennent dans les
//!   capacités déjà réservées et n'allouent plus après le premier bloc.
//! - **Ordre libre** : [`OrdreLocal`] dit dans quel ordre passent les étages.
//!   [`OrdreLocal::HISTORIQUE`] reproduit l'ordre d'avant ; [`OrdreLocal::depuis_chaine`]
//!   lit l'ordre d'une [`ChaineTess`] (la clé `zone_{id}_tess_chain` arrive
//!   avec T02 ; le branchement par zone suit dans ce lot).
//! - **« Bypass all »** : aucun étage n'est appelé, l'entrée ressort à l'octet.
//! - **Adaptateurs provisoires `f32` ↔ `f64`** : l'égaliseur, le convolveur et
//!   le crossfeed (natifs tiers compris) gardent leurs moteurs `f32` sous leurs
//!   verrous historiques. Chaque adaptateur convertit à SA frontière, par
//!   morceaux d'une pile fixe (aucune allocation de l'adaptateur), et rend des
//!   `f64`. T05, T06 et T09b les remplacent par des étages `f64` natifs.
//!   Parce que tout ce qu'un moteur `f32` rend est exactement représentable en
//!   `f64`, la chaîne est **bit pour bit** celle d'avant tant que les étages
//!   sont ceux d'avant (non-régression prouvée par les témoins ci-dessous).
//! - **Repli mono** : premier étage en `f64` natif. Il reste le DERNIER des
//!   étages (#2362 ; rattaché à `channel-remap`, place `last_plugin`).
//! - **Points d'insertion de T07** : les étages fixes (headroom avec
//!   ReplayGain et gain de zone en tête, volume au rendu et dither en queue)
//!   ne sont pas des étages de cette liste : headroom se placera AVANT
//!   [`executer_chaine_locale`], le volume puis l'[`EtapeFinale`](crate::tess::EtapeFinale)
//!   APRÈS, à la place de la frontière `f32` du producteur.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::warn;

use crate::audio::convolver::Convolver;
use crate::audio::crossfeed::CrossfeedProcessor;
use crate::audio::eq::EqProcessor;
use crate::tess::{
    ChaineTess, Disposition, ErreurEtage, EtageTess, Executeur, FormatFlux, etage::verifier_tampon,
};

/// Un étage de la chaîne locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EtageLocal {
    Egaliseur = 0,
    Convolution = 1,
    Crossfeed = 2,
    RepliMono = 3,
}

const NB_ETAGES: usize = 4;

/// L'ordre des étages locaux et le « Bypass all ».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrdreLocal {
    etages: [EtageLocal; NB_ETAGES],
    bypass_all: bool,
}

// `depuis_ids`, `depuis_chaine` et les lecteurs servent à la lecture de
// l'ordre par zone (T02, branchement dans la PR suivante de T03) ; d'ici là,
// seuls les témoins les appellent.
#[cfg_attr(not(test), allow(dead_code))]
impl OrdreLocal {
    /// L'ordre d'avant TESS : égaliseur, convolveur, crossfeed, repli mono.
    pub(crate) const HISTORIQUE: Self = Self {
        etages: [
            EtageLocal::Egaliseur,
            EtageLocal::Convolution,
            EtageLocal::Crossfeed,
            EtageLocal::RepliMono,
        ],
        bypass_all: false,
    };

    /// L'ordre d'une suite d'identifiants de greffons TESS.
    ///
    /// - Les trois façades d'égaliseur visent le même moteur ; la famille
    ///   crossfeed (`crossfeed`, `crossfeed-*`, `crosstalk`) vise le
    ///   `CrossfeedProcessor`, qui porte aussi les natifs tiers ; la première
    ///   occurrence fixe la place.
    /// - Un étage que la liste ne nomme pas garde sa place historique
    ///   relative, après ceux qu'elle nomme : un moteur posé par les anciennes
    ///   clés n'est jamais retiré en silence.
    /// - Le repli mono reste DERNIER (`channel-remap` est `last_plugin`).
    pub(crate) fn depuis_ids<'a>(ids: impl IntoIterator<Item = &'a str>, bypass_all: bool) -> Self {
        let mut etages = [EtageLocal::RepliMono; NB_ETAGES];
        let mut n = 0;
        let mut pris = [false; NB_ETAGES];
        let mut placer = |e: EtageLocal, etages: &mut [EtageLocal; NB_ETAGES]| {
            if e != EtageLocal::RepliMono && !pris[e as usize] {
                pris[e as usize] = true;
                etages[n] = e;
                n += 1;
            }
        };
        for id in ids {
            let e = match id {
                "equalizer-graphic" | "equalizer-parametric" | "equalizer-headphone" => {
                    EtageLocal::Egaliseur
                }
                "convolution" | "room-correction" => EtageLocal::Convolution,
                "crossfeed" | "crosstalk" => EtageLocal::Crossfeed,
                _ if id.starts_with("crossfeed-") => EtageLocal::Crossfeed,
                _ => continue,
            };
            placer(e, &mut etages);
        }
        for e in Self::HISTORIQUE.etages {
            placer(e, &mut etages);
        }
        etages[NB_ETAGES - 1] = EtageLocal::RepliMono;
        Self { etages, bypass_all }
    }

    /// L'ordre réel d'une chaîne TESS de zone.
    pub(crate) fn depuis_chaine(chaine: &ChaineTess) -> Self {
        Self::depuis_ids(
            chaine.plugins.iter().map(|m| m.id.as_str()),
            chaine.bypass_all,
        )
    }

    pub(crate) fn etages(&self) -> [EtageLocal; NB_ETAGES] {
        self.etages
    }

    pub(crate) fn bypass_all(&self) -> bool {
        self.bypass_all
    }
}

/// Taille de la pile de conversion d'un adaptateur `f32` (32 Kio).
const MORCEAU: usize = 8_192;

/// Fait passer `tampon` par un moteur `f32`, par morceaux de trames entières
/// convertis dans une pile fixe. N'alloue pas. Les moteurs d'origine
/// (convolveur, égaliseur, crossfeed) rendent la même suite quel que soit le
/// découpage de leurs appels.
fn par_morceaux_f32(tampon: &mut [f64], canaux: usize, mut moteur: impl FnMut(&mut [f32])) {
    let canaux = canaux.max(1);
    let pas = (MORCEAU / canaux) * canaux;
    let mut pile = [0.0_f32; MORCEAU];
    for morceau in tampon.chunks_mut(pas) {
        let f = &mut pile[..morceau.len()];
        for (d, s) in f.iter_mut().zip(morceau.iter()) {
            *d = *s as f32;
        }
        moteur(f);
        for (d, s) in morceau.iter_mut().zip(f.iter()) {
            *d = f64::from(*s);
        }
    }
}

/// Adaptateur provisoire de l'égaliseur (`f32`) : à porter en `f64` par T06.
struct AdaptateurEgaliseur<'a> {
    moteur: Option<&'a Mutex<Option<EqProcessor>>>,
    canaux: usize,
    contourne: bool,
}

impl EtageTess for AdaptateurEgaliseur<'_> {
    fn id(&self) -> &str {
        "equalizer"
    }
    fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        self.canaux = usize::from(e.channels);
        Ok(Disposition::sur_place(e, max, 0))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        if self.contourne {
            return Ok(());
        }
        if let Some(m) = self.moteur
            && let Ok(mut e) = m.lock()
            && let Some(p) = e.as_mut()
        {
            par_morceaux_f32(&mut t[..trames * self.canaux], self.canaux, |f| {
                p.process_interleaved(f);
            });
            // #5215 — l'égaliseur coupé a fini son fondu et ne garde aucun
            // préampli : on le retire, la chaîne redevient celle d'un EQ absent.
            if p.est_neutre_au_repos() {
                *e = None;
            }
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        self.contourne
    }
    fn definir_bypass(&mut self, b: bool) {
        self.contourne = b;
    }
}

/// Adaptateur provisoire du convolveur (`f32`) : à porter en `f64` par T09b.
/// Le contournement avec retard conservé viendra avec le moteur `f64` (ligne
/// de retard préparée) ; d'ici là, cet adaptateur n'est jamais contourné.
struct AdaptateurConvolution<'a> {
    moteur: &'a Mutex<Option<Convolver>>,
    canaux: usize,
}

impl EtageTess for AdaptateurConvolution<'_> {
    fn id(&self) -> &str {
        "convolution"
    }
    fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        self.canaux = usize::from(e.channels);
        let latence = self
            .moteur
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|c| c.latency_frames() as u32))
            .unwrap_or(0);
        Ok(Disposition::sur_place(e, max, latence))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        if let Ok(mut conv) = self.moteur.lock()
            && let Some(c) = conv.as_mut()
        {
            par_morceaux_f32(&mut t[..trames * self.canaux], self.canaux, |f| {
                c.process_interleaved(f);
            });
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

/// Adaptateur provisoire du crossfeed et des natifs tiers (`f32`), stéréo
/// seulement : à porter en `f64` par T05/T14.
struct AdaptateurCrossfeed<'a> {
    moteur: &'a Mutex<Option<CrossfeedProcessor>>,
    canaux: usize,
    contourne: bool,
}

impl EtageTess for AdaptateurCrossfeed<'_> {
    fn id(&self) -> &str {
        "crossfeed"
    }
    fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        self.canaux = usize::from(e.channels);
        Ok(Disposition::sur_place(e, max, 0))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        // Le crossfeed travaille sur la différence des voies : stéréo seulement
        // (`stereo_only`), comme avant.
        if self.contourne || self.canaux != 2 {
            return Ok(());
        }
        if let Ok(mut cf) = self.moteur.lock()
            && let Some(c) = cf.as_mut()
        {
            par_morceaux_f32(&mut t[..trames * 2], 2, |f| c.process_interleaved(f));
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        self.contourne
    }
    fn definir_bypass(&mut self, b: bool) {
        self.contourne = b;
    }
}

/// Le repli mono, en `f64` natif. Stéréo seulement ; DERNIER des étages
/// (#2362 : les autres ont besoin de leur contexte stéréo).
struct RepliMono<'a> {
    actif: &'a AtomicBool,
    canaux: usize,
    contourne: bool,
}

impl EtageTess for RepliMono<'_> {
    fn id(&self) -> &str {
        "channel-remap"
    }
    fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        self.canaux = usize::from(e.channels);
        Ok(Disposition::sur_place(e, max, 0))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        if self.contourne || self.canaux != 2 || !self.actif.load(Ordering::Relaxed) {
            return Ok(());
        }
        for trame in t[..trames * 2].as_chunks_mut::<2>().0 {
            let m = (trame[0] + trame[1]) * 0.5;
            trame[0] = m;
            trame[1] = m;
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        self.contourne
    }
    fn definir_bypass(&mut self, b: bool) {
        self.contourne = b;
    }
}

thread_local! {
    /// L'exécuteur du fil : réutilisé d'un bloc à l'autre, il n'alloue plus
    /// une fois ses capacités réservées.
    static EXECUTEUR: RefCell<Executeur> = RefCell::new(Executeur::new());
}

/// Les moteurs de la chaîne locale, sous leurs verrous historiques.
pub(super) struct MoteursLocaux<'a> {
    /// `None` : la vidange n'a pas d'égaliseur à faire passer (l'ordre
    /// historique le place avant le convolveur).
    pub(super) eq: Option<&'a Mutex<Option<EqProcessor>>>,
    pub(super) convolver: &'a Mutex<Option<Convolver>>,
    pub(super) crossfeed: &'a Mutex<Option<CrossfeedProcessor>>,
    pub(super) pure_bypass: &'a AtomicBool,
    pub(super) mono_downmix: &'a AtomicBool,
}

impl MoteursLocaux<'_> {
    /// Ce que la chaîne ne traite pas, elle n'a rien à en rendre : DoP, PURE,
    /// « Bypass all ».
    fn exemptee(&self, ordre: OrdreLocal, dop: bool) -> bool {
        dop || self.pure_bypass.load(Ordering::Relaxed) || ordre.bypass_all
    }

    /// Fait passer `samples` par les étages `ordre.etages()[debut..]`.
    fn executer(&self, ordre: OrdreLocal, debut: usize, samples: &mut [f64], channels: u16) {
        let canaux = usize::from(channels.max(1));
        let trames = samples.len() / canaux;
        if trames == 0 {
            return;
        }
        let mut eq = AdaptateurEgaliseur {
            moteur: self.eq,
            canaux,
            contourne: false,
        };
        let mut conv = AdaptateurConvolution {
            moteur: self.convolver,
            canaux,
        };
        let mut cf = AdaptateurCrossfeed {
            moteur: self.crossfeed,
            canaux,
            contourne: false,
        };
        let mut mono = RepliMono {
            actif: self.mono_downmix,
            canaux,
            contourne: false,
        };
        let mut prets: [Option<&mut dyn EtageTess>; NB_ETAGES] = [
            Some(&mut eq),
            Some(&mut conv),
            Some(&mut cf),
            Some(&mut mono),
        ];
        let mut etages: [&mut dyn EtageTess; NB_ETAGES] = ordre.etages.map(|e| {
            prets[e as usize]
                .take()
                .expect("OrdreLocal est une permutation des quatre étages")
        });
        let etages = &mut etages[debut.min(NB_ETAGES)..];
        // La cadence n'entre dans aucun étage local : 0 = « non fournie ».
        let format = FormatFlux {
            channels: channels.max(1),
            sample_rate_hz: 0,
        };
        let resultat = EXECUTEUR.with(|cellule| {
            let mut secours;
            let mut emprunt = cellule.try_borrow_mut();
            let ex: &mut Executeur = match emprunt {
                Ok(ref mut e) => e,
                Err(_) => {
                    secours = Executeur::new();
                    &mut secours
                }
            };
            ex.preparer(etages, format, trames, false)?;
            ex.traiter(etages, samples, trames).map(|_| ())
        });
        if let Err(error) = resultat {
            warn!(%error, channels, trames, "tess_local_chain_failed");
        }
    }
}

/// TESS T03 — la chaîne DSP locale, en `f64`, dans l'ordre `ordre`.
pub(super) fn executer_chaine_locale(
    samples: &mut [f64],
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) {
    if moteurs.exemptee(ordre, dop) {
        return;
    }
    moteurs.executer(ordre, 0, samples, channels);
}

/// Fin de piste : ce que le convolveur retient encore, passé par les étages
/// qui le SUIVENT dans `ordre` (crossfeed et repli mono dans l'ordre
/// historique), pour que la queue sonne comme le reste.
pub(super) fn vidanger_chaine_locale(
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) -> Vec<f64> {
    if moteurs.exemptee(ordre, dop) {
        return Vec::new();
    }
    let queue = match moteurs.convolver.lock() {
        Ok(mut c) => match c.as_mut() {
            Some(conv) => conv.flush(),
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    let mut queue: Vec<f64> = queue.into_iter().map(f64::from).collect();
    if queue.is_empty() {
        return queue;
    }
    let apres = ordre
        .etages
        .iter()
        .position(|e| *e == EtageLocal::Convolution)
        .map_or(NB_ETAGES, |i| i + 1);
    moteurs.executer(ordre, apres, &mut queue, channels);
    queue
}

/// La frontière PROVISOIRE `f64` → `f32` du producteur local : l'anneau et
/// l'étage de conversion portent encore des `f32`. L'engagement du contrat
/// (§ 8 : anneau en `f64`, quantification unique au rendu) la retire.
/// Tant que les étages actifs sont des moteurs `f32`, elle est exacte.
pub(super) fn frontiere_f32(samples: &[f64]) -> Vec<f32> {
    samples.iter().map(|&x| x as f32).collect()
}

#[cfg(test)]
#[path = "executeur_tess_tests.rs"]
mod tests;
