//! TESS T04 — la chaîne DSP RÉSEAU passe par l'exécuteur TESS, en `f64`, avec
//! UNE seule quantification à la fin.
//!
//! Avant ce lot, chaque étage du porteur réseau ([`StreamingDsp`]) et du
//! transcodage de fichier (`transcodage::transcode_source_to_file_avec_crossfeed`)
//! lisait des OCTETS PCM et rendait des OCTETS PCM : le ReplayGain, l'égaliseur,
//! le convolveur, le crossfeed et la compensation de niveau requantifiaient
//! chacun le signal à la profondeur du flux (avec un dither TPDF pour trois
//! d'entre eux). Une zone DLNA avec ReplayGain, égaliseur et compensation
//! entendait donc TROIS quantifications et trois bruits de dither.
//!
//! Ici, le bloc est décodé UNE fois en `f64` (exact à 16, 24 et 32 bits), passe
//! par les étages, puis par la [`QuantificationPcm`] de T03 — la seule sortie du
//! `f64`, et la seule quantification :
//!
//! ```text
//! octets → f64 → ReplayGain (f64) → [égaliseur → convolveur → crossfeed]
//!        → (amorce de latence retirée) → compensation (f64) → QuantificationPcm → octets
//! ```
//!
//! - **Exécuteur** : les trois greffons (égaliseur, convolveur, crossfeed et
//!   natifs tiers) passent par l'[`Executeur`] de T03, dans l'ordre historique
//!   (`transcode_source_to_file` : égaliseur, convolveur, crossfeed). La vidange de fin de flux (queue du convolveur)
//!   passe par les étages qui le SUIVENT, comme le contrat le fixe.
//! - **Étages fixes en `f64` natif** : le ReplayGain (point d'insertion du
//!   headroom de T07, AVANT l'exécuteur) et la compensation (#5071, APRÈS,
//!   juste avant la quantification) sont de simples gains, appliqués en `f64`.
//! - **Moteurs de référence en `f64` natif (T06, T09b)** : l'égaliseur, le
//!   crossfeed et le convolveur reçoivent le tampon `f64` de l'exécuteur tel
//!   quel ; un natif tiers `F32` est converti à SA frontière (T05). Aucune
//!   AUTRE conversion `f32` ni entière n'existe entre deux étages
//!   (`crate::tess::sonde_f32` le vérifie en test).
//! - **Bit-perfect** : sans étage actif, le porteur ne touche pas un octet
//!   ([`StreamingDsp::is_active`]) ; avec des étages qui rendent des mots
//!   exacts, la quantification les rend à l'identité, sans dither (règle de
//!   [`QuantificationPcm`]).
//! - **Dither** : un générateur TPDF continu par porteur (graine fixée à la
//!   première préparation) ; même piste, même porteur neuf : mêmes octets, ce
//!   qui garde le cache de transcodage exact.
//! - **Compensation, piste entière** : la piste ENTIÈRE est un seul bloc
//!   (#5071) — le gain est borné par la crête de la piste traitée. La sortie de
//!   la chaîne est donc gardée jusqu'à la fin de la piste ([`Stockage`]) : en
//!   `f32` quand chaque échantillon y est EXACT (dernier étage actif = le
//!   convolveur `f32`), en `f64` sinon. Aucune précision n'est jamais perdue.
//!   Depuis T06, l'égaliseur et le crossfeed rendent des `f64` : une piste
//!   compensée est alors gardée en `f64`, place réservée d'emblée, jusqu'à
//!   [`SEUIL_STOCKAGE_F64`] échantillons ; au-delà, en `f32` (garde-fou
//!   mémoire pour les longues pistes en haute résolution).

use crate::audio::convolver::Convolver;
use crate::audio::crossfeed::CrossfeedProcessor;
use crate::audio::ecretage::CompteurDEcretage;
use crate::audio::eq::EqProcessor;
use crate::tess::etage::verifier_tampon;
use crate::tess::{
    Disposition, ErreurEtage, EtageTess, EtapeFinale, Executeur, FormatFlux, QuantificationPcm,
    SortieFinale,
};

use super::StreamingDsp;

/// Taille maximale, en échantillons, d'une tranche traitée d'un seul tenant
/// (512 Kio en `f64`). Un tampon du relais progressif y tient : chaque moteur
/// reçoit alors un seul appel par tampon, comme avant. La piste entière (bras
/// DASH et HTTPS, transcodage de fichier) est découpée : la mémoire reste
/// bornée.
pub(super) const TRANCHE: usize = 1 << 16;

/// Octets par échantillon d'une profondeur que la chaîne sait lire.
fn largeur(bit_depth: u16) -> Option<usize> {
    match bit_depth {
        16 => Some(2),
        24 => Some(3),
        32 => Some(4),
        _ => None,
    }
}

/// Décodage EXACT d'octets PCM entiers petit-boutistes en `f64`, pleine
/// échelle ±1,0 (échelle 2^(n−1), la même que la quantification).
pub(super) fn decoder_f64(octets: &[u8], bit_depth: u16, sortie: &mut [f64]) {
    match bit_depth {
        16 => {
            for (d, s) in sortie.iter_mut().zip(octets.as_chunks::<2>().0) {
                *d = f64::from(i16::from_le_bytes(*s)) / 32_768.0;
            }
        }
        24 => {
            for (d, s) in sortie.iter_mut().zip(octets.as_chunks::<3>().0) {
                let v = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8;
                *d = f64::from(v) / 8_388_608.0;
            }
        }
        32 => {
            for (d, s) in sortie.iter_mut().zip(octets.as_chunks::<4>().0) {
                *d = f64::from(i32::from_le_bytes(*s)) / 2_147_483_648.0;
            }
        }
        _ => {}
    }
}

/// Écriture des mots cadrés à gauche de la [`QuantificationPcm`] en octets
/// petit-boutistes de `bit_depth` bits.
fn ecrire_mots(mots: &[i32], bit_depth: u16, octets: &mut [u8]) {
    match bit_depth {
        16 => {
            for (s, m) in octets.as_chunks_mut::<2>().0.iter_mut().zip(mots) {
                *s = ((m >> 16) as i16).to_le_bytes();
            }
        }
        24 => {
            for (s, m) in octets.as_chunks_mut::<3>().0.iter_mut().zip(mots) {
                let b = (m >> 8).to_le_bytes();
                *s = [b[0], b[1], b[2]];
            }
        }
        32 => {
            for (s, m) in octets.as_chunks_mut::<4>().0.iter_mut().zip(mots) {
                *s = m.to_le_bytes();
            }
        }
        _ => {}
    }
}

/// Pleine échelle et rail haut, en LSB, d'une profondeur.
fn rails(bit_depth: u16) -> (f64, f64) {
    let echelle = (1u64 << (bit_depth - 1)) as f64;
    (echelle, echelle - 1.0)
}

/// Nombre d'échantillons d'une tranche : au plus [`TRANCHE`], en trames
/// entières.
fn pas_de_tranche(canaux: usize) -> usize {
    let canaux = canaux.max(1);
    ((TRANCHE / canaux) * canaux).max(canaux)
}

/// L'égaliseur (graphique, paramétrique, casque AutoEQ et OPRA), en `f64`
/// natif depuis TESS T06 : aucun aller-retour par `f32`.
struct AdaptateurEgaliseur<'a> {
    moteur: &'a mut Option<EqProcessor>,
    canaux: usize,
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
        // Un égaliseur coupé qui a fini son fondu ne fait rien (#5215).
        if let Some(p) = self.moteur.as_mut()
            && !p.est_neutre_au_repos()
        {
            p.process_interleaved_f64(&mut t[..trames * self.canaux]);
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

/// TESS T09b — l'étage de convolution en `f64` natif (le moteur calcule en
/// double précision et reçoit le tampon de la chaîne sans conversion).
///
/// Le moteur rend la trame `i` à la position `i + latence` : la latence est
/// DÉCLARÉE à l'exécuteur, l'hôte retire l'amorce en tête de flux, et la queue
/// sort par [`EtageTess::vidanger`] (des zéros poussés dans le moteur).
struct AdaptateurConvolution<'a> {
    moteur: &'a mut Option<Convolver>,
    canaux: usize,
    /// Trames de queue encore à rendre par la vidange de ce flux.
    queue_restante: &'a mut Option<usize>,
}

impl EtageTess for AdaptateurConvolution<'_> {
    fn id(&self) -> &str {
        "convolution"
    }
    fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        self.canaux = usize::from(e.channels);
        let latence = self.moteur.as_ref().map_or(0, Convolver::latency_frames);
        Ok(Disposition::sur_place(
            e,
            max,
            u32::try_from(latence).unwrap_or(u32::MAX),
        ))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        if let Some(c) = self.moteur.as_mut() {
            self.queue_restante
                .get_or_insert_with(|| c.latency_frames());
            c.process_interleaved_f64(&mut t[..trames * self.canaux]);
        }
        Ok(())
    }
    fn vidanger(&mut self, sortie: &mut [f64]) -> Result<usize, ErreurEtage> {
        let Some(c) = self.moteur.as_mut() else {
            return Ok(0);
        };
        let canaux = self.canaux.max(1);
        let restant = self
            .queue_restante
            .get_or_insert_with(|| c.latency_frames());
        let trames = (*restant).min(sortie.len() / canaux);
        if trames == 0 {
            return Ok(0);
        }
        let n = trames * canaux;
        sortie[..n].fill(0.0);
        c.process_interleaved_f64(&mut sortie[..n]);
        *restant -= trames;
        Ok(trames)
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
    moteur: &'a mut Option<CrossfeedProcessor>,
    /// Le porteur dit-il traiter de la stéréo ? (`StreamingDsp::channels`,
    /// voir [`StreamingDsp::crossfeed_executable`]).
    stereo: bool,
    canaux: usize,
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
        if !self.stereo || self.canaux != 2 {
            return Ok(());
        }
        if let Some(c) = self.moteur.as_mut() {
            c.process_interleaved_f64(&mut t[..trames * 2]);
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

/// rc14 — le réglage des enceintes (`speaker-setup`), dernier greffon de
/// l'exécuteur. Ses délais sont VOULUS (alignement des voies) : il annonce
/// une latence nulle à l'exécuteur, pour que l'hôte ne les retire pas comme
/// l'amorce du convolveur. Le chemin du signal publie le plus grand délai.
struct AdaptateurEnceintes<'a> {
    moteur: &'a mut Option<crate::audio::reglage_enceintes::ProcesseurEnceintes>,
    canaux: usize,
    cadence: u32,
}

impl EtageTess for AdaptateurEnceintes<'_> {
    fn id(&self) -> &str {
        crate::audio::reglage_enceintes::ID_GREFFON
    }
    fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        self.canaux = usize::from(e.channels);
        Ok(Disposition::sur_place(e, max, 0))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        verifier_tampon(t.len(), trames, self.canaux as u16)?;
        if let Some(p) = self.moteur.as_mut() {
            p.traiter(
                &mut t[..trames * self.canaux],
                self.canaux as u16,
                self.cadence,
            );
        }
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

/// rc14 — délai au-delà duquel la ligne à retard gardée d'une piste réseau
/// n'est plus reprise par la suivante (lecture arrêtée, autre écoute).
const FRAICHEUR_ENCEINTES: std::time::Duration = std::time::Duration::from_secs(10);

fn enceintes_gardees() -> &'static std::sync::Mutex<
    std::collections::HashMap<
        i64,
        (
            std::time::Instant,
            crate::audio::reglage_enceintes::ProcesseurEnceintes,
        ),
    >,
> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                i64,
                (
                    std::time::Instant,
                    crate::audio::reglage_enceintes::ProcesseurEnceintes,
                ),
            >,
        >,
    > = std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

/// rc14 — fin complète d'un flux réseau : sa ligne à retard est gardée pour
/// la piste suivante de la zone.
pub(super) fn garder_les_enceintes(
    zone: i64,
    p: crate::audio::reglage_enceintes::ProcesseurEnceintes,
) {
    if let Ok(mut t) = enceintes_gardees().lock() {
        t.insert(zone, (std::time::Instant::now(), p));
    }
}

/// rc14 — le processeur du réglage des enceintes d'un flux réseau de la
/// zone (`canaux` voies à `cadence` Hz).
///
/// **Enchaînement sans blanc** : quand la piste précédente de la zone vient
/// de finir COMPLÈTEMENT au même format, sa ligne à retard est reprise. La
/// queue de la voie retardée (les `d` dernières trames) sort alors en tête
/// de la piste suivante, à sa place : ni zéros en tête, ni queue perdue,
/// longueur de chaque piste inchangée. Le délai n'est PAS déclaré comme une
/// latence à retirer (l'exécuteur retirerait l'amorce de TOUTES les voies, la
/// voie lointaine perdrait ses premières trames).
///
/// Un réglage changé entre deux pistes passe par le fondu du processeur.
/// Sinon (première piste, saut, arrêt, autre format) : processeur neuf. `None`
/// si le greffon ne toucherait pas ce flux.
pub(super) fn reprendre_les_enceintes(
    zone: i64,
    reglages: Option<crate::audio::reglage_enceintes::ReglagesEnceintes>,
    canaux: u16,
    cadence: u32,
) -> Option<crate::audio::reglage_enceintes::ProcesseurEnceintes> {
    let garde = enceintes_gardees()
        .lock()
        .ok()
        .and_then(|mut t| t.remove(&zone))
        .filter(|(quand, p)| {
            quand.elapsed() <= FRAICHEUR_ENCEINTES && p.format_vu() == Some((canaux, cadence))
        })
        .map(|(_, p)| p);
    let reglages = reglages.filter(|r| !r.est_neutre(usize::from(canaux)))?;
    Some(match garde {
        Some(mut p) => {
            p.definir(reglages);
            p
        }
        None => crate::audio::reglage_enceintes::ProcesseurEnceintes::new(reglages),
    })
}

/// Au-delà de ce nombre d'échantillons (32 Mi, 256 Mo en `f64`), une piste
/// gardée pour la compensation l'est en `f32` : garde-fou mémoire (relecture
/// de #9414). Une piste de 10 min en 192 kHz stéréo (230 M échantillons)
/// prendrait 1,8 Go en `f64`, plus que la mémoire d'un Pi de 2 Go ; en `f32`,
/// réservée d'emblée, 920 Mo. En deçà (≈ 6 min à 44,1 kHz stéréo, 1 min 27 s
/// à 192 kHz), la piste garde ses 64 bits : la compensation reste UNE
/// quantification de la référence `f64` (#5071, T04).
pub(super) const SEUIL_STOCKAGE_F64: usize = 32 << 20;

/// La sortie de la chaîne gardée jusqu'à la fin de l'appel, quand la
/// compensation doit voir la crête de TOUT le bloc avant d'appliquer son
/// gain.
///
/// - Sous [`SEUIL_STOCKAGE_F64`] : en `f32` tant que chaque échantillon y est
///   exact (dernier étage : le convolveur `f32`), en `f64` dès qu'un seul ne
///   l'est plus — aucune perte.
/// - Au-delà : en `f32`, arrondi au plus proche (erreur relative ≤ 2⁻²⁴, sous
///   le dither TPDF de la quantification finale à 16 bits, de l'ordre d'un
///   demi-LSB à 24 bits en pleine échelle) : la mémoire prime.
///
/// La place est RÉSERVÉE d'après la longueur connue de l'appel (la piste
/// entière est dans le tampon) : ni réallocation, ni capacité doublée.
enum Stockage {
    F32 {
        v: Vec<f32>,
        arrondi_permis: bool,
        total: usize,
    },
    F64(Vec<f64>),
}

impl Stockage {
    /// Un stockage pour `total` échantillons attendus.
    fn pour(total: usize, seuil_f64: usize) -> Self {
        Self::F32 {
            v: Vec::with_capacity(total),
            arrondi_permis: total > seuil_f64,
            total,
        }
    }

    fn ajouter(&mut self, y: &[f64]) {
        if let Self::F32 {
            v,
            arrondi_permis,
            total,
        } = self
        {
            if *arrondi_permis || y.iter().all(|&x| f64::from(x as f32) == x) {
                v.extend(y.iter().map(|&x| x as f32));
                return;
            }
            let mut large: Vec<f64> = Vec::with_capacity((*total).max(v.len() + y.len()));
            large.extend(v.iter().map(|&x| f64::from(x)));
            *self = Self::F64(large);
        }
        if let Self::F64(v) = self {
            v.extend_from_slice(y);
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::F32 { v, .. } => v.len(),
            Self::F64(v) => v.len(),
        }
    }

    fn crete(&self) -> f64 {
        match self {
            Self::F32 { v, .. } => v.iter().fold(0.0_f64, |m, &x| m.max(f64::from(x).abs())),
            Self::F64(v) => v.iter().fold(0.0_f64, |m, &x| m.max(x.abs())),
        }
    }

    /// Copie `[debut, debut + n)` en `f64` dans `sortie`.
    fn lire(&self, debut: usize, sortie: &mut [f64]) {
        match self {
            Self::F32 { v, .. } => {
                for (d, s) in sortie.iter_mut().zip(&v[debut..]) {
                    *d = f64::from(*s);
                }
            }
            Self::F64(v) => sortie.copy_from_slice(&v[debut..debut + sortie.len()]),
        }
    }
}

/// L'état TESS d'un porteur réseau : l'exécuteur, la quantification finale et
/// leurs tampons, réutilisés d'un appel à l'autre.
#[derive(Default)]
pub(super) struct EtatTess {
    executeur: Executeur,
    /// La quantification finale et le format pour lequel elle est préparée ;
    /// `None` tant que rien n'a été quantifié. Préparée UNE fois par porteur :
    /// son générateur TPDF est continu d'un appel à l'autre.
    quantification: Option<(QuantificationPcm, FormatFlux, usize)>,
    entree: Vec<f64>,
    mots: Vec<i32>,
    /// Trames de queue que le convolveur doit encore rendre à la vidange.
    queue_restante: Option<usize>,
}

/// Ce que fait un appel de la chaîne.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Passage {
    /// Piste ENTIÈRE : même longueur en sortie, queue du convolveur comprise,
    /// puis moteurs remis à zéro pour une piste suivante.
    Piste,
    /// Un tampon d'un flux SUCCESSIF : l'amorce de latence est retirée au fil
    /// des tampons (le tampon peut raccourcir), la queue attend [`Passage::Fin`].
    Flux,
    /// Fin du flux : la queue retenue par la latence, passée par les étages
    /// qui suivent le convolveur.
    Fin,
}

/// Où va la sortie de la chaîne pendant un appel.
enum Puits {
    /// Quantifiée tout de suite et écrite en octets à partir de `ecrits`.
    Direct { ecrits: usize },
    /// Gardée pour la compensation : la crête du bloc ENTIER d'abord.
    Garde(Stockage),
}

/// Quantifie `y` (un multiple de `canaux`) et l'écrit dans `octets` à partir
/// de `*ecrits`. La SEULE quantification. Rien n'est écrit au-delà du
/// tampon : la somme des longueurs rendues ne dépasse jamais celle reçue.
fn quantifier_vers(
    quant: &mut QuantificationPcm,
    mots: &mut Vec<i32>,
    canaux: usize,
    bit_depth: u16,
    y: &[f64],
    octets: &mut [u8],
    ecrits: &mut usize,
) {
    let Some(w) = largeur(bit_depth) else {
        return;
    };
    #[cfg(test)]
    tests::sonde::noter(y);
    for morceau in y.chunks(pas_de_tranche(canaux)) {
        if mots.len() < morceau.len() {
            mots.resize(morceau.len(), 0);
        }
        let trames = morceau.len() / canaux;
        let fait = quant.finaliser(
            morceau,
            trames,
            SortieFinale::Pcm {
                echantillons: &mut mots[..morceau.len()],
                bits: bit_depth as u8,
            },
        );
        match fait {
            Ok(n) => {
                let fin = (*ecrits + n * w).min(octets.len());
                let n = (fin - *ecrits) / w;
                ecrire_mots(&mots[..n], bit_depth, &mut octets[*ecrits..fin]);
                *ecrits = fin;
            }
            Err(error) => {
                tracing::warn!(%error, bit_depth, "tess_network_quantization_failed");
                return;
            }
        }
    }
}

/// Prépare (une fois par porteur et par format) la quantification finale.
fn preparer_quantification(
    q: &mut Option<(QuantificationPcm, FormatFlux, usize)>,
    format: FormatFlux,
    max_trames: usize,
) -> &mut QuantificationPcm {
    let a_refaire = !matches!(q, Some((_, f, m)) if *f == format && *m == max_trames);
    if a_refaire {
        let mut quant = QuantificationPcm::new(true);
        if let Err(error) = quant.preparer(format, max_trames) {
            tracing::warn!(%error, "tess_network_quantization_prepare_failed");
        }
        *q = Some((quant, format, max_trames));
    }
    &mut q.as_mut().expect("préparée ci-dessus").0
}

/// Le ReplayGain en `f64` (étage fixe, en tête). Il ne sature plus : la seule
/// borne est celle de la quantification finale. Il COMPTE toujours, dans le
/// registre du processus, ce qu'il pousse au-delà de la pleine échelle (la
/// condition de l'ancien `apply_gain_pcm`), pour que `dsp_ecretage` dise encore
/// quand le gain demandé dépasse le rail.
fn appliquer_replaygain(
    x: &mut [f64],
    facteur: f64,
    bit_depth: u16,
    base: u64,
    compteur: &mut CompteurDEcretage,
) {
    let (echelle, max) = rails(bit_depth);
    for (i, s) in x.iter_mut().enumerate() {
        *s *= facteur;
        let v = *s * echelle;
        if v > max {
            compteur.noter_ecrete(base + i as u64, v - max, v / echelle);
        } else if v < -echelle {
            compteur.noter_ecrete(base + i as u64, -echelle - v, -v / echelle);
        }
    }
    compteur.noter_vus(x.len() as u64);
}

impl StreamingDsp {
    /// Le nombre de canaux entrelacés du PCM que ce porteur voit. Le porteur
    /// le dit (`channels`) ; un porteur bâti sans le dire (`Default`, gain seul)
    /// prend celui du convolveur, sinon traite les échantillons un par un —
    /// ReplayGain et compensation sont des gains, insensibles à l'entrelacement.
    pub(super) fn canaux_du_flux(&self) -> usize {
        if self.channels > 0 {
            return usize::from(self.channels);
        }
        self.convolver
            .as_ref()
            .map(Convolver::channels)
            .filter(|&c| c > 0)
            .unwrap_or(1)
    }

    /// TESS T04 — fait passer `pcm` par la chaîne en `f64`, avec UNE seule
    /// quantification (voir le module et [`Passage`]). Rend le nombre
    /// d'octets écrits à partir de `pcm[0]`.
    ///
    /// - [`Passage::Piste`] et [`Passage::Flux`] lisent les trames entières de
    ///   `pcm` ; un reste qui n'est pas une trame entière n'est pas touché.
    /// - [`Passage::Fin`] ne lit rien : `pcm` reçoit la queue (l'appelant le
    ///   dimensionne à la latence).
    /// - Une profondeur que la chaîne ne sait pas lire laisse tout intact
    ///   (rend la longueur lue, comme si rien ne s'était passé).
    pub(super) fn traiter_par_tess(
        &mut self,
        pcm: &mut [u8],
        bit_depth: u16,
        passage: Passage,
    ) -> usize {
        let Some(w) = largeur(bit_depth) else {
            return if passage == Passage::Fin {
                0
            } else {
                pcm.len()
            };
        };
        let canaux = self.canaux_du_flux();
        let trame = w * canaux;
        let utiles = if passage == Passage::Fin {
            0
        } else {
            pcm.len() / trame * trame
        };
        if utiles == 0 && passage != Passage::Fin {
            return 0;
        }
        let echantillons = utiles / w;
        let pas = pas_de_tranche(canaux);
        let format = FormatFlux {
            channels: u16::try_from(canaux).unwrap_or(u16::MAX),
            sample_rate_hz: self.sample_rate,
        };
        let max_trames = pas / canaux;

        let Self {
            replaygain,
            eq,
            convolver,
            crossfeed,
            channels,
            compensation,
            convolveur_reste_a_jeter,
            tess,
            enceintes,
            sample_rate,
            ..
        } = self;
        let EtatTess {
            executeur,
            quantification,
            entree,
            mots,
            queue_restante,
        } = tess;
        let a_une_latence = convolver.is_some();
        let mut a_eq = AdaptateurEgaliseur { moteur: eq, canaux };
        let mut a_conv = AdaptateurConvolution {
            moteur: &mut *convolver,
            canaux,
            queue_restante: &mut *queue_restante,
        };
        let mut a_cf = AdaptateurCrossfeed {
            moteur: crossfeed,
            stereo: *channels == 2,
            canaux,
        };
        let mut a_enc = AdaptateurEnceintes {
            moteur: &mut *enceintes,
            canaux,
            cadence: *sample_rate,
        };
        let mut etages: [&mut dyn EtageTess; 4] = [&mut a_eq, &mut a_conv, &mut a_cf, &mut a_enc];
        if let Err(error) = executeur.preparer(&mut etages, format, max_trames, false) {
            tracing::warn!(%error, canaux, "tess_network_chain_prepare_failed");
            return utiles;
        }
        let latence = usize::try_from(executeur.latence_totale_trames()).unwrap_or(usize::MAX);
        let mut a_jeter = *convolveur_reste_a_jeter.get_or_insert(latence);
        let quant = preparer_quantification(quantification, format, max_trames);
        if entree.len() < pas {
            entree.resize(pas, 0.0);
        }
        let facteur = replaygain.filter(|f| (f - 1.0).abs() >= 1e-9);
        let mut compteur_rg = CompteurDEcretage::default();
        let mut puits = if compensation.is_some() {
            Puits::Garde(Stockage::pour(echantillons, SEUIL_STOCKAGE_F64))
        } else {
            Puits::Direct { ecrits: 0 }
        };

        // Une sortie de la chaîne : l'amorce de latence retirée, puis vers le
        // puits. En `Direct`, l'écriture reste derrière la lecture : la sortie
        // d'une tranche ne dépasse jamais son entrée, déjà copiée.
        let emettre = |y: &[f64],
                       a_jeter: &mut usize,
                       puits: &mut Puits,
                       quant: &mut QuantificationPcm,
                       mots: &mut Vec<i32>,
                       pcm: &mut [u8]| {
            let trames = y.len() / canaux;
            let jetees = (*a_jeter).min(trames);
            *a_jeter -= jetees;
            let y = &y[jetees * canaux..];
            if y.is_empty() {
                return;
            }
            match puits {
                Puits::Direct { ecrits } => {
                    quantifier_vers(quant, mots, canaux, bit_depth, y, pcm, ecrits);
                }
                Puits::Garde(stock) => stock.ajouter(y),
            }
        };

        let mut lu = 0;
        while lu < echantillons {
            let n = pas.min(echantillons - lu);
            let x = &mut entree[..n];
            decoder_f64(&pcm[lu * w..(lu + n) * w], bit_depth, x);
            if let Some(f) = facteur {
                appliquer_replaygain(x, f, bit_depth, lu as u64, &mut compteur_rg);
            }
            match executeur.traiter(&mut etages, x, n / canaux) {
                Ok(sortie) => emettre(
                    sortie.echantillons,
                    &mut a_jeter,
                    &mut puits,
                    quant,
                    mots,
                    pcm,
                ),
                Err(error) => {
                    tracing::warn!(%error, canaux, "tess_network_chain_failed");
                }
            }
            lu += n;
        }

        if passage != Passage::Flux && a_une_latence {
            let mut vidange = |y: &[f64]| emettre(y, &mut a_jeter, &mut puits, quant, mots, pcm);
            if let Err(error) = executeur.vidanger(&mut etages, &mut vidange) {
                tracing::warn!(%error, canaux, "tess_network_chain_drain_failed");
            }
        }

        if facteur.is_some() {
            let premier = crate::audio::ecretage::REGISTRE
                .replaygain
                .absorber(&CompteurDEcretage::default(), &compteur_rg);
            if premier {
                crate::audio::ecretage::dire_premier(
                    crate::audio::ecretage::EtageEcretant::ReplayGain,
                    crate::audio::ecretage::Portee::Processus,
                    &compteur_rg,
                );
            }
        }

        // La compensation (#5071) : la crête du bloc entier, puis le gain, en
        // `f64`, puis la quantification.
        let ecrits = match puits {
            Puits::Direct { ecrits } => ecrits,
            Puits::Garde(stock) => {
                let mut ecrits = 0;
                if let Some(comp) = compensation.as_mut() {
                    comp.retenir_pour_crete(stock.crete());
                    let mut debut = 0;
                    while debut < stock.len() {
                        let n = pas.min(stock.len() - debut);
                        let y = &mut entree[..n];
                        stock.lire(debut, y);
                        comp.appliquer_f64(y, bit_depth);
                        quantifier_vers(quant, mots, canaux, bit_depth, y, pcm, &mut ecrits);
                        debut += n;
                    }
                }
                ecrits
            }
        };

        // Fin de piste ou de flux : moteurs à zéro, latence réarmée.
        if passage != Passage::Flux {
            if let Some(c) = convolver.as_mut() {
                c.reset();
            }
            // rc14 — une piste entière ne garde pas la queue de la précédente.
            if let Some(e) = enceintes.as_mut() {
                e.reinitialiser();
            }
            *queue_restante = None;
            *convolveur_reste_a_jeter = None;
        } else {
            *convolveur_reste_a_jeter = Some(a_jeter);
        }
        // Piste entière : somme des longueurs rendues = somme des longueurs
        // reçues (l'amorce retirée en tête est rendue par la queue).
        if passage == Passage::Piste && ecrits != utiles {
            tracing::warn!(ecrits, utiles, "tess_network_track_length_mismatch");
        }
        ecrits
    }
}

#[cfg(test)]
#[path = "chaine_reseau_tess_tests.rs"]
mod tests;
