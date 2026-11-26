//! Fil 3107 (rc13.91) — la cadence d'une radio décodée pour un lecteur, et
//! le rééchantillonneur qui l'y amène.
//!
//! ## Le défaut
//!
//! easyradio.bg diffuse en FLAC 192 kHz. Vers un Sonos (ZPS12), Tune servait
//! un WAV 192 kHz : la cadence de sortie de la radio n'était que
//! `renderer_safe_wav_rate(source)`, qui relève sous 44,1 kHz mais ne
//! plafonne JAMAIS. Le catalogue sait pourtant que tout Sonos s'arrête à
//! 48 kHz (#5194), et les pistes comme la fréquence fixe en tiennent compte.
//! Le Sonos lisait l'en-tête (durée annoncée 46:36, soit 768 000 octets/s),
//! coupait la connexion au bout de 11 ms et passait en STOPPED, en boucle à
//! chaque relance automatique de la station.
//!
//! ## Ce qui change
//!
//! - [`cadence_de_sortie_radio`] : sans fréquence fixe, la cadence est bornée
//!   par le plafond de la zone (réglage de zone et catalogue, le même que
//!   borne la fréquence fixe). Elle descend DANS SA FAMILLE quand elle le peut
//!   (192 → 48 kHz, 88,2 → 44,1 kHz) : un rapport entier, le plus simple et le
//!   plus propre à filtrer.
//! - [`ReechantillonneurRadio`] : le sinc de rubato, AVEC ÉTAT, gardé d'un
//!   paquet à l'autre pendant tout le flux (reconnexions compatibles
//!   comprises). Il remplace `simple_resample` (interpolation linéaire à deux
//!   points, sans filtre anti-repliement, remise à zéro à chaque paquet) sur
//!   TOUT changement de cadence de la radio : descente, fréquence fixe et
//!   relèvement des HE-AAC à 22,05 kHz.
//!
//! ## Le départ du flux : un fondu de 10 ms
//!
//! Un sinc qui part du silence et reçoit une station prise au milieu d'une
//! onde voit une MARCHE : il y répond par l'oscillation de Gibbs, jusqu'à
//! 9 % AU-DESSUS de la source (mesuré : 0,5436 pour une source à 0,5). Les
//! 10 premières millisecondes d'un rééchantillonneur neuf montent donc en
//! cosinus surélevé (gain ≤ 1 partout) : plus de marche, plus de dépassement.
//!
//! `simple_resample` ne reste qu'en repli, si rubato refuse de construire
//! son noyau (cadence nulle ou aberrante) : c'est dit au journal
//! (`radio_reechantillonneur_lineaire_f3107`).
use tracing::warn;

/// Le plafond de cadence d'une zone, quand il en a un, et la cadence que la
/// radio servira : la fréquence fixe si elle est réglée (déjà bornée par
/// `frequence_fixe::active`), sinon la cadence sûre de la station ramenée
/// sous le plafond.
#[must_use]
pub(crate) fn cadence_de_sortie_radio(
    source: u32,
    frequence_fixe: Option<u32>,
    plafond: Option<u32>,
) -> u32 {
    if let Some(fixe) = frequence_fixe {
        return fixe;
    }
    let sure = super::renderer_safe_wav_rate(source);
    match plafond.filter(|p| *p > 0) {
        Some(p) if sure > p => descendre_dans_la_famille(sure, p),
        _ => sure,
    }
}

/// Divise la cadence par deux tant qu'elle dépasse le plafond et qu'elle
/// reste une cadence audio (≥ 44,1 kHz) : 192 000 → 48 000,
/// 176 400 → 44 100. Si la famille ne tombe pas sous le plafond, le plafond
/// lui-même.
fn descendre_dans_la_famille(cadence: u32, plafond: u32) -> u32 {
    let mut c = cadence;
    while c > plafond && c.is_multiple_of(2) && c / 2 >= 44_100 {
        c /= 2;
    }
    if c <= plafond { c } else { plafond }
}

/// Le rééchantillonneur d'une radio : un sinc rubato à état, ou, faute de
/// mieux, l'interpolation linéaire d'avant.
pub(crate) struct ReechantillonneurRadio {
    de: u32,
    vers: u32,
    canaux: u16,
    sinc: Option<rubato::Async<f32>>,
    /// Les trames reçues qui ne remplissent pas encore un bloc de rubato :
    /// elles attendent le paquet suivant, au lieu d'être complétées par du
    /// silence (ce qui ferait un clic à chaque paquet).
    reste: Vec<f32>,
    /// Trames d'entrée reçues depuis la construction (diagnostic et témoins).
    trames_recues: u64,
    /// Longueur du fondu d'entrée, en trames (10 ms à la cadence source).
    fondu: u64,
}

/// La durée du fondu d'entrée d'un rééchantillonneur neuf.
const FONDU_D_ENTREE_MS: u64 = 10;

impl ReechantillonneurRadio {
    pub(crate) fn nouveau(de: u32, vers: u32, canaux: u16) -> Self {
        let sinc = match crate::audio::resample::new_streaming_resampler(de, vers, canaux) {
            Ok(r) => Some(r),
            Err(error) => {
                warn!(
                    de,
                    vers,
                    canaux,
                    %error,
                    "radio_reechantillonneur_lineaire_f3107 — le sinc n'a pas pu être construit, \
                     repli sur l'interpolation linéaire"
                );
                None
            }
        };
        Self {
            de,
            vers,
            canaux,
            sinc,
            reste: Vec::new(),
            trames_recues: 0,
            fondu: (u64::from(de) * FONDU_D_ENTREE_MS / 1_000).max(1),
        }
    }

    /// Convient-il encore à ce format ? Sinon il faut en bâtir un autre.
    pub(crate) fn convient(&self, de: u32, vers: u32, canaux: u16) -> bool {
        (self.de, self.vers, self.canaux) == (de, vers, canaux)
    }

    /// Le nom du noyau, pour le journal.
    pub(crate) fn noyau(&self) -> &'static str {
        if self.sinc.is_some() {
            "sinc"
        } else {
            "lineaire"
        }
    }

    #[cfg(test)]
    pub(crate) fn trames_recues(&self) -> u64 {
        self.trames_recues
    }

    /// Rééchantillonne un paquet entrelacé. La sortie peut être plus courte
    /// que `entree × rapport` (voire vide) : ce qui manque est retenu et sort
    /// avec le paquet suivant, sans rien perdre ni dupliquer.
    pub(crate) fn traiter(&mut self, entree: &[f32]) -> Vec<f32> {
        let canaux = usize::from(self.canaux.max(1));
        let premiere = self.trames_recues;
        self.trames_recues += (entree.len() / canaux) as u64;
        let fondue: Vec<f32>;
        let entree = if premiere < self.fondu {
            fondue = self.fondre(entree, premiere, canaux);
            &fondue[..]
        } else {
            entree
        };
        if self.sinc.is_none() {
            return crate::audio::simple_resample(entree, self.de, self.vers, self.canaux);
        }
        let debut = std::time::Instant::now();
        let sortie = crate::audio::resample::rubato_resample_chunk(
            &mut self.sinc,
            entree,
            self.canaux,
            false,
            &mut self.reste,
        );
        crate::audio::resample::surveiller_le_cout(
            "radio",
            debut,
            sortie.len(),
            self.canaux,
            self.vers,
        );
        sortie
    }
}

impl ReechantillonneurRadio {
    /// Le fondu d'entrée en cosinus surélevé, appliqué aux trames
    /// `premiere..` du flux qui tombent dans ses 10 premières millisecondes.
    fn fondre(&self, entree: &[f32], premiere: u64, canaux: usize) -> Vec<f32> {
        let mut v = entree.to_vec();
        for (i, trame) in v.chunks_mut(canaux).enumerate() {
            let k = premiere + i as u64;
            if k >= self.fondu {
                break;
            }
            let x = k as f64 / self.fondu as f64;
            let gain = (0.5 - 0.5 * (std::f64::consts::PI * x).cos()) as f32;
            for s in trame {
                *s *= gain;
            }
        }
        v
    }
}

/// Tient le rééchantillonneur de la radio à jour pour une connexion : aucun
/// quand la cadence ne change pas, le MÊME (état gardé) quand le format est
/// inchangé, un neuf sinon.
pub(crate) fn tenir_le_reechantillonneur(
    place: &mut Option<ReechantillonneurRadio>,
    de: u32,
    vers: u32,
    canaux: u16,
) {
    if de == vers {
        *place = None;
        return;
    }
    if place.as_ref().is_some_and(|r| r.convient(de, vers, canaux)) {
        return;
    }
    *place = Some(ReechantillonneurRadio::nouveau(de, vers, canaux));
}

#[cfg(test)]
#[path = "radio_cadence_f3107_tests.rs"]
mod tests;
