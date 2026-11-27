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
//! - **Moteurs de référence en `f64` natif (T06, T09b)** : l'égaliseur
//!   (graphique, paramétrique, casque), le crossfeed et le convolveur reçoivent
//!   le tampon `f64` de l'exécuteur tel quel, sous leurs verrous historiques ;
//!   un natif tiers `F32` est converti à SA frontière par l'hôte natif (T05).
//!   Le témoin `crate::tess::sonde_f32` compte ces frontières. Par rapport à
//!   la chaîne `f32` d'avant, seul l'arrondi change (un à la frontière au lieu
//!   d'un par étage) : quelques ULP `f32`, sous −126 dBFS (témoins ci-dessous).
//! - **Repli mono** : premier étage en `f64` natif. Il reste le DERNIER des
//!   étages (#2362 ; rattaché à `channel-remap`, place `last_plugin`).
//! - **Points d'insertion de T07** : les étages fixes (headroom avec
//!   ReplayGain et gain de zone en tête, volume au rendu et dither en queue)
//!   ne sont pas des étages de cette liste : headroom se placera AVANT
//!   [`executer_chaine_locale`], le volume puis l'[`EtapeFinale`](crate::tess::EtapeFinale)
//!   APRÈS, à la place de la frontière `f32` du producteur.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

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
    ///   crossfeed (`crossfeed`, `crossfeed-*`, `crosstalk`) et tout natif
    ///   tiers (T14) visent le `CrossfeedProcessor`, qui les porte ; la
    ///   première occurrence fixe la place.
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
                // rc14 — la correction de pièce est une façade de l'égaliseur
                // (`zone_{id}_eq_profile`, D1) : elle vit dans son moteur.
                "equalizer-graphic"
                | "equalizer-parametric"
                | "equalizer-headphone"
                | "room-correction" => EtageLocal::Egaliseur,
                "convolution" => EtageLocal::Convolution,
                "crossfeed" | "crosstalk" => EtageLocal::Crossfeed,
                _ if id.starts_with("crossfeed-") => EtageLocal::Crossfeed,
                // TESS T14 — un natif tiers (absent du catalogue de
                // référence) vit dans l'étage casque : il le place, comme
                // les membres de la famille du crossfeed.
                _ if est_un_natif_tiers(id) => EtageLocal::Crossfeed,
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

    /// L'ordre réel d'une chaîne TESS de zone : celui des greffons COCHÉS
    /// (un maillon décoché ne traite rien et ne place aucun étage).
    ///
    /// rc14 — une chaîne que l'utilisateur n'a jamais réordonnée donne
    /// [`Self::HISTORIQUE`] : la migration range les greffons au rang de la
    /// lecture d'avant (`migration::rang` : égaliseurs, convolution,
    /// crossfeed, natifs, repli mono, réaffectation), le même ordre.
    ///
    /// Les égaliseurs cochés forment UN étage (un seul moteur, leurs bandes
    /// en série, `tess::egaliseurs_en_serie`), placé au rang du premier.
    pub(crate) fn depuis_chaine(chaine: &ChaineTess) -> Self {
        Self::depuis_ids(
            chaine
                .plugins
                .iter()
                .filter(|m| m.enabled)
                .map(|m| m.id.as_str()),
            chaine.bypass_all,
        )
    }

    /// rc14 — l'ordre posé pour la sortie locale dont `cle` est le drapeau
    /// PURE (`LocalOutput::pure_bypass`, propre à chaque sortie) ;
    /// [`Self::HISTORIQUE`] si aucun ne l'a été. « Bypass all » n'est pas
    /// porté ici : il passe par `pure_bypass` (`traitements_contournes`).
    pub(crate) fn de_la_sortie(cle: &AtomicBool) -> Self {
        if ORDRES_POSES.load(Ordering::Acquire) == 0 {
            return Self::HISTORIQUE;
        }
        ordres()
            .read()
            .ok()
            .and_then(|t| t.get(&adresse(cle)).copied())
            .unwrap_or(Self::HISTORIQUE)
    }

    /// rc14 — pose l'ordre de la chaîne TESS de la zone sur cette sortie
    /// (voir [`Self::de_la_sortie`]). L'ordre historique n'est pas retenu.
    pub(crate) fn poser_pour_la_sortie(cle: &AtomicBool, ordre: Self) {
        let ordre = Self {
            bypass_all: false,
            ..ordre
        };
        if let Ok(mut t) = ordres().write() {
            if ordre == Self::HISTORIQUE {
                t.remove(&adresse(cle));
            } else {
                t.insert(adresse(cle), ordre);
            }
            ORDRES_POSES.store(t.len(), Ordering::Release);
        }
    }

    /// rc14 — la sortie disparaît : son ordre aussi.
    pub(crate) fn oublier_la_sortie(cle: &AtomicBool) {
        Self::poser_pour_la_sortie(cle, Self::HISTORIQUE);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn etages(&self) -> [EtageLocal; NB_ETAGES] {
        self.etages
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn bypass_all(&self) -> bool {
        self.bypass_all
    }
}

/// rc14 — l'ordre TESS de chaque sortie locale, par l'adresse de son
/// drapeau PURE (un `Arc<AtomicBool>` propre à la sortie, stable tant
/// qu'elle vit). Seuls les ordres différents de l'historique y sont.
static ORDRES_POSES: AtomicUsize = AtomicUsize::new(0);

fn ordres() -> &'static RwLock<HashMap<usize, OrdreLocal>> {
    static T: OnceLock<RwLock<HashMap<usize, OrdreLocal>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn adresse(cle: &AtomicBool) -> usize {
    std::ptr::from_ref(cle) as usize
}

/// TESS T14 — `id` est-il un greffon natif tiers : un identifiant que l'hôte
/// accepte pour un paquet tiers, que le catalogue de référence ne connaît
/// pas, et dont un fournisseur DSP est RÉELLEMENT chargé
/// ([`crate::audio::natifs_tiers::fournisseur_dsp`]). Un identifiant inconnu
/// de l'hôte natif (greffon WASM, paquet absent ou en échec) ne déplace pas
/// l'étage casque.
fn est_un_natif_tiers(id: &str) -> bool {
    crate::tess::catalogue_de_reference().get(id).is_none()
        && crate::audio::natifs_tiers::fournisseur_dsp(id).is_some()
}

/// Taille maximale, en échantillons, d'une tranche traitée d'un seul tenant
/// (256 Kio en `f32`, 512 Kio en `f64`). Un bloc du producteur (quelques
/// dizaines de milliers d'échantillons) tient dans une tranche : les moteurs
/// reçoivent alors EXACTEMENT les appels d'avant (même découpage, donc mêmes
/// fins de fondu). Seul un bloc plus grand — la piste entière du chemin
/// compressé — est découpé, et la mémoire reste bornée.
pub(super) const TRANCHE: usize = 1 << 16;

thread_local! {
    /// Tampon `f64` du décodage et de l'entrée `f32` : même règle.
    static TAMPON_F64: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
    /// rc14 — la copie traitée du fondu de sortie de chaîne (PURE).
    static TAMPON_TRAITE: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
}

/// Nombre d'échantillons d'une tranche : au plus [`TRANCHE`], en trames
/// entières.
fn pas_de_tranche(canaux: usize) -> usize {
    let canaux = canaux.max(1);
    ((TRANCHE / canaux) * canaux).max(canaux)
}

/// Prête un tampon de `n` échantillons (au plus [`TRANCHE`]) tiré du tampon
/// du fil ; un emprunt réentrant (jamais en lecture ordinaire) reçoit un
/// tampon neuf.
fn avec_tampon<T: Copy + Default>(
    cle: &'static std::thread::LocalKey<RefCell<Vec<T>>>,
    n: usize,
    f: impl FnOnce(&mut [T]),
) {
    cle.with(|cellule| match cellule.try_borrow_mut() {
        Ok(mut v) => {
            if v.len() < n {
                v.resize(n, T::default());
            }
            f(&mut v[..n]);
        }
        Err(_) => f(&mut vec![T::default(); n]),
    });
}

/// L'égaliseur (graphique, paramétrique, casque AutoEQ et OPRA : un seul
/// moteur), en `f64` natif depuis TESS T06 : le tampon de l'exécuteur lui est
/// passé tel quel, sans aucun aller-retour par `f32`.
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
            p.process_interleaved_f64(&mut t[..trames * self.canaux]);
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

/// TESS T09b — l'étage de convolution, en `f64` natif : le moteur
/// ([`Convolver`]) calcule en double précision et reçoit le tampon de la
/// chaîne tel quel, sans conversion. Sa latence (`latency_frames()`, un bloc)
/// est ANNONCÉE à l'exécuteur. Sa queue de fin de piste passe par
/// `Convolver::flush_f64` ([`vidanger_chaine_locale`]), pas par la vidange de
/// l'exécuteur. Le changement de filtre flux en cours est un fondu du moteur
/// lui-même (`Convolver::remplacer_en_fondu`, posé par la sortie locale).
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
            .and_then(|c| c.as_ref().map(Convolver::latency_frames))
            .unwrap_or(0);
        Ok(Disposition::sur_place(
            e,
            max,
            u32::try_from(latence).unwrap_or(u32::MAX),
        ))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        if let Ok(mut conv) = self.moteur.lock()
            && let Some(c) = conv.as_mut()
        {
            c.process_interleaved_f64(&mut t[..trames * self.canaux]);
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

/// Le crossfeed (v1 intégré, en `f64` natif depuis TESS T06) et les natifs
/// tiers (convertis à LEUR frontière s'ils sont des paquets `F32`, T05),
/// stéréo seulement.
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
            c.process_interleaved_f64(&mut t[..trames * 2]);
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
/// rc14 (sécurité auditive, relecture de #9556) — on a QUITTÉ PURE (ou
/// « Bypass all ») pendant le fondu de la chaîne sortante : la chaîne est
/// retirée de l'égaliseur coupé qui la portait (sinon il ne se mettait
/// jamais au repos : seul le chemin PURE la fond), et son crossfeed, pris à
/// l'entrée en PURE, revient TOUT DE SUITE dans la sortie, historique
/// compris, si rien ne l'y a remplacé — au lieu d'attendre la piste
/// suivante. Appelé par `LocalOutput::set_pure_bypass(false)` et, au cas où
/// l'exécuteur tenait la chaîne hors du verrou à ce moment, à chaque bloc
/// hors PURE.
pub(super) fn abandonner_la_chaine_sortante(
    eq: &Mutex<Option<EqProcessor>>,
    crossfeed: &Mutex<Option<CrossfeedProcessor>>,
) {
    let rendu = match eq.lock() {
        Ok(mut g) => match g.as_mut() {
            Some(p) if p.fond_une_chaine() => p.abandonner_la_chaine_sortante(),
            _ => return,
        },
        Err(_) => return,
    };
    if let Some(cf) = rendu
        && let Ok(mut emplacement) = crossfeed.lock()
        && emplacement.is_none()
    {
        *emplacement = Some(cf);
    }
}

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

    /// Un étage touche-t-il le signal ? Même décision que l'ancien
    /// `apply_local_dsp` : un égaliseur posé (même neutre, pour que la chaîne
    /// le retire au repos comme avant), un convolveur posé, et en stéréo un
    /// crossfeed posé ou le repli mono. Sinon, aucune copie n'est faite.
    fn un_etage_agit(&self, channels: u16) -> bool {
        self.eq
            .is_some_and(|m| m.lock().map(|g| g.is_some()).unwrap_or(false))
            || self.convolver.lock().map(|g| g.is_some()).unwrap_or(false)
            || (channels == 2
                && (self.mono_downmix.load(Ordering::Relaxed)
                    || self.crossfeed.lock().map(|g| g.is_some()).unwrap_or(false)))
    }

    /// La chaîne laissera-t-elle ce bloc intact ?
    fn sans_effet(&self, ordre: OrdreLocal, channels: u16, dop: bool) -> bool {
        (self.exemptee(ordre, dop) && !self.transition_d_entree(dop))
            || !self.un_etage_agit(channels)
    }

    /// rc14 (sécurité auditive, décision du 25/11) — sous PURE ou « Bypass
    /// all » (jamais en DoP), l'égaliseur COUPÉ de la transition joue encore
    /// tant qu'il n'est pas au repos : la transition d'entrée (`LocalOutput::set_pure_bypass`)
    /// le fait rejoindre le niveau d'avant l'égaliseur au lieu d'un saut au
    /// signal sec, puis il se retire et la chaîne redevient bit-perfect.
    /// Seul cet étage joue alors ([`Self::executer`]).
    fn transition_d_entree(&self, dop: bool) -> bool {
        !dop && self.eq.is_some_and(|m| {
            m.lock().is_ok_and(|g| {
                g.as_ref()
                    .is_some_and(|p| p.est_neutre() && !p.est_neutre_au_repos())
            })
        })
    }

    /// rc14 (sécurité auditive, décision du 25/11) — l'entrée en PURE ou en
    /// « Bypass all » retire aussi la convolution, le crossfeed et le repli
    /// mono : d'un bloc à l'autre, un saut de niveau (la réserve d'une
    /// convolution, −6 à −12 dB) et une discontinuité du signal (un clic).
    /// Le temps de [`crate::audio::eq::ChaineSortante`], ces étages jouent
    /// encore sur une COPIE du bloc (après l'égaliseur de la transition), et
    /// la sortie passe en fondu linéaire du traité au sec. Le sec est
    /// multiplié par le niveau de la chaîne, MESURÉ sur la musique (rapport
    /// des énergies, interpolé dans le bloc : aucune marche), jamais au-dessus
    /// de 1. Fondu fini, ce niveau rejoint le gain gardé de l'égaliseur coupé
    /// (`EqProcessor::terminer_la_chaine_sortante`), qui remonte vers le
    /// niveau d'avant le traitement, jamais au-delà.
    ///
    /// **Latence de la convolution (relecture de #9556).** Le traité sort
    /// `Convolver::latency_frames()` trames après le sec (un bloc de 1 024
    /// trames : 21 ms à 48 kHz). Le sec n'est PAS aligné sur ce retard, à
    /// dessein : sous PURE, la sortie rend le flux sans ce retard (le bras
    /// natif bit-perfect ne passe même plus par l'exécuteur), donc ces
    /// trames doivent disparaître quelque part. Les jeter DANS ce fondu de
    /// 300 ms est le raccord le plus doux : aligner le sec ne ferait que
    /// déplacer la coupure à la fin du fondu, en un saut de 21 ms (un clic)
    /// ou un second raccord. Pour la sécurité auditive, c'est sans risque :
    /// le traité et le sec × `gain` ont la même énergie (c'est ainsi que
    /// `gain` est mesuré, sur des énergies insensibles au décalage), et la
    /// somme `(1 − t)·traité + t·sec·gain` ne dépasse jamais, en amplitude,
    /// `(1 − t)·|traité| + t·|sec·gain|` : aucun échantillon au-dessus du
    /// niveau d'avant. Le seul effet est un CREUX passager, jamais une
    /// hausse : −6 dB au milieu du fondu pour le sinus de 1 kHz du banc de
    /// mesure (déphasé de 120° par les 1 024 trames), environ −3 dB en
    /// moyenne sur une musique large bande, au pire une fréquence en
    /// opposition de phase qui s'efface un instant. Le crossfeed et le repli
    /// mono n'ont pas de latence.
    fn fondre_la_chaine_sortante(
        &self,
        ordre: OrdreLocal,
        samples: &mut [f64],
        canaux: usize,
        trames: usize,
    ) {
        let Some(moteur) = self.eq else {
            return;
        };
        // La chaîne sort du verrou le temps du bloc : la convolution prend
        // le sien, jamais les deux à la fois.
        let Some(mut chaine) = moteur
            .lock()
            .ok()
            .and_then(|mut g| g.as_mut().and_then(EqProcessor::prendre_la_chaine_sortante))
        else {
            return;
        };
        let n = trames * canaux;
        avec_tampon(&TAMPON_TRAITE, n, |traite| {
            traite.copy_from_slice(&samples[..n]);
            for etage in ordre.etages {
                match etage {
                    EtageLocal::Egaliseur => {}
                    EtageLocal::Convolution if chaine.convolution => {
                        let mut conv = AdaptateurConvolution {
                            moteur: self.convolver,
                            canaux,
                        };
                        let _ = conv.traiter_sur_place(traite, trames);
                    }
                    EtageLocal::Convolution => {}
                    EtageLocal::Crossfeed => {
                        if canaux == 2
                            && let Some(cf) = chaine.crossfeed.as_mut()
                        {
                            cf.process_interleaved_f64(traite);
                        }
                    }
                    EtageLocal::RepliMono => {
                        if canaux == 2 && chaine.mono {
                            for trame in traite.as_chunks_mut::<2>().0 {
                                let m = (trame[0] + trame[1]) * 0.5;
                                trame[0] = m;
                                trame[1] = m;
                            }
                        }
                    }
                }
            }
            chaine.energie_traitee += traite.iter().map(|x| x * x).sum::<f64>();
            chaine.energie_seche += samples[..n].iter().map(|x| x * x).sum::<f64>();
            let depart = chaine.gain;
            let arrivee = chaine.gain_mesure();
            for f in 0..trames {
                let t = ((chaine.fait + f + 1) as f64 / chaine.total as f64).min(1.0);
                let g = depart + (arrivee - depart) * ((f + 1) as f64 / trames as f64);
                for c in 0..canaux {
                    let i = f * canaux + c;
                    samples[i] = traite[i] + (samples[i] * g - traite[i]) * t;
                }
            }
            chaine.gain = arrivee;
            chaine.fait += trames;
        });
        if let Ok(mut g) = moteur.lock() {
            match g.as_mut() {
                Some(p) if chaine.fait >= chaine.total => {
                    p.terminer_la_chaine_sortante(
                        chaine.gain,
                        crate::audio::eq::REMONTEE_RAPIDE_TESS_MS,
                    );
                    // Rien à garder (niveau mesuré à l'unité) : identité
                    // exacte, l'étage se retire tout de suite.
                    if p.est_neutre_au_repos() {
                        *g = None;
                    }
                }
                Some(p) => p.rendre_la_chaine_sortante(chaine),
                None => {}
            }
        }
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
        // rc14 — transition d'entrée en PURE / « Bypass all » : l'égaliseur
        // seul, les autres étages restent contournés.
        if self.pure_bypass.load(Ordering::Relaxed) || ordre.bypass_all {
            if debut == 0 {
                let _ = eq.traiter_sur_place(samples, trames);
                self.fondre_la_chaine_sortante(ordre, samples, canaux, trames);
            }
            return;
        }
        // rc14 (relecture de #9556) — PURE quitté pendant le fondu de la
        // chaîne sortante : elle reprend sa place dans le flux.
        if let Some(m) = self.eq {
            abandonner_la_chaine_sortante(m, self.crossfeed);
        }
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

/// TESS T03 — la chaîne DSP locale sur un tampon `f64` déjà décodé (la
/// forme des témoins ; la production passe par [`decoder_par_la_chaine_tess`]).
#[cfg(test)]
pub(super) fn executer_chaine_locale(
    samples: &mut [f64],
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) {
    if moteurs.sans_effet(ordre, channels, dop) {
        return;
    }
    moteurs.executer(ordre, 0, samples, channels);
}

/// TESS T03 — l'entrée `f32` (flux compressé décodé en bloc, témoins) : la
/// chaîne en `f64`, tranche par tranche dans le tampon du fil, résultat remis
/// en place. Ni copie de la piste, ni copie du tout si aucun étage n'agit.
pub(super) fn executer_chaine_locale_f32(
    samples: &mut [f32],
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) {
    if moteurs.sans_effet(ordre, channels, dop) {
        return;
    }
    for morceau in samples.chunks_mut(pas_de_tranche(usize::from(channels))) {
        avec_tampon(&TAMPON_F64, morceau.len(), |large| {
            for (d, s) in large.iter_mut().zip(morceau.iter()) {
                *d = f64::from(*s);
            }
            moteurs.executer(ordre, 0, large, channels);
            for (d, s) in morceau.iter_mut().zip(large.iter()) {
                *d = *s as f32;
            }
        });
    }
}

/// TESS T03 — le geste du producteur : octets PCM → chaîne en `f64` →
/// frontière `f32` de l'anneau. Une seule allocation (le bloc `f32` rendu),
/// comme avant ; le `f64` vit dans le tampon du fil, bloc par bloc. Si
/// aucun étage n'agit, c'est exactement l'ancien décodage `f32`.
pub(super) fn decoder_par_la_chaine_tess(
    octets: &[u8],
    bit_depth: u16,
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) -> Vec<f32> {
    if moteurs.sans_effet(ordre, channels, dop) {
        return super::pcm_bytes_to_f32(octets, bit_depth);
    }
    let largeur = super::octets_par_echantillon(bit_depth);
    let total = octets.len() / largeur;
    let mut sortie = Vec::with_capacity(total);
    let pas = pas_de_tranche(usize::from(channels));
    for morceau in octets[..total * largeur].chunks(pas * largeur) {
        avec_tampon(&TAMPON_F64, morceau.len() / largeur, |large| {
            super::decoder_f64_dans(morceau, bit_depth, large);
            moteurs.executer(ordre, 0, large, channels);
            sortie.extend(large.iter().map(|&x| x as f32));
        });
    }
    sortie
}

/// Fin de piste : ce que le convolveur retient encore, passé par les étages
/// qui le SUIVENT dans `ordre` (crossfeed et repli mono dans l'ordre
/// historique), pour que la queue sonne comme le reste.
pub(super) fn vidanger_chaine_locale(
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) -> Vec<f32> {
    vidanger_chaine_locale_f64(moteurs, ordre, channels, dop)
        .into_iter()
        .map(|x| x as f32)
        .collect()
}

/// [`vidanger_chaine_locale`] avant la frontière `f32` du producteur (T09b :
/// la queue de la convolution reste en `f64` jusqu'au bout de la chaîne).
pub(super) fn vidanger_chaine_locale_f64(
    moteurs: &MoteursLocaux<'_>,
    ordre: OrdreLocal,
    channels: u16,
    dop: bool,
) -> Vec<f64> {
    if moteurs.exemptee(ordre, dop) {
        return Vec::new();
    }
    // T09b — la queue sort du moteur en `f64` et suit la chaîne sans repasser
    // par le `f32`.
    let mut queue = match moteurs.convolver.lock() {
        Ok(mut c) => match c.as_mut() {
            Some(conv) => conv.flush_f64(),
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    if queue.is_empty() {
        return Vec::new();
    }
    let apres = ordre
        .etages
        .iter()
        .position(|e| *e == EtageLocal::Convolution)
        .map_or(NB_ETAGES, |i| i + 1);
    moteurs.executer(ordre, apres, &mut queue, channels);
    queue
}

/// La frontière PROVISOIRE `f64` → `f32` du producteur local (dans la
/// production, elle est faite en ligne par [`decoder_par_la_chaine_tess`]) : l'anneau et
/// l'étage de conversion portent encore des `f32`. L'engagement du contrat
/// (§ 8 : anneau en `f64`, quantification unique au rendu) la retire.
/// Tant que les étages actifs sont des moteurs `f32`, elle est exacte.
#[cfg(test)]
pub(super) fn frontiere_f32(samples: &[f64]) -> Vec<f32> {
    samples.iter().map(|&x| x as f32).collect()
}

#[cfg(test)]
#[path = "executeur_tess_tests.rs"]
pub(super) mod tests;
