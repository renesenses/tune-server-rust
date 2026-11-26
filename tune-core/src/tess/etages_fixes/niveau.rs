//! Le **bilan de niveau** : ce que chaque étage fixe retire ou rend, compté
//! UNE fois.
//!
//! # Ce qui baisse le niveau
//!
//! - le **headroom** de TESS (auto −3 dB dès qu'un traitement est effectif,
//!   ou manuel) ;
//! - la **réserve des greffons** : leur gain MOYEN, tel que chacun le
//!   déclare (réserve anti-écrêtage de l'égaliseur, Side retiré par le
//!   crossfeed : les nombres de #4685) ;
//! - le ReplayGain et le gain de zone, quand ils sont négatifs : ce sont des
//!   choix de NIVEAU, pas des protections, et la compensation ne les rend
//!   pas.
//!
//! # Pas de double compte (#4594, étude TESS L11)
//!
//! 1. **L'automatique ne s'empile pas sur une réserve.** −3 dB est un
//!    MINIMUM de protection : un égaliseur qui réserve déjà 12 dB (mesuré sur
//!    AutoEq, #4594) en couvre plus. La part automatique est donc
//!    `min(0, −3 − réserve)` : −12 dB de réserve ⇒ 0 dB de plus, et non
//!    −15 dB ; −1 dB de réserve ⇒ −2 dB de plus. Un headroom MANUEL est un
//!    choix explicite : il s'applique tel quel.
//! 2. **La réserve de 3 dB du suréchantillonneur disparaît** sous TESS :
//!    l'automatique la couvre (à retirer dans T11a).
//! 3. **La compensation rend chaque baisse de protection une fois, et une
//!    seule** : `−(headroom appliqué + gain moyen des greffons)`. Elle est
//!    rendue par le VOLUME au rendu, rabotée à l'unité avec lui (comme
//!    aujourd'hui, #4685/#5069) : elle ne peut pas écrêter, et sous la pleine
//!    échelle le niveau perçu est celui de la zone sans traitement. Elle ne
//!    rend ni le ReplayGain ni le gain de zone.
//!
//! # Ce qui va où
//!
//! - [`BilanDeNiveau::gain_d_entree_db`] → l'étage [`super::EtageHeadroom`]
//!   (≤ 0 dB : l'entrée ne pousse jamais) ;
//! - [`BilanDeNiveau::gain_au_rendu_db`] (compensation + part positive du
//!   ReplayGain et du gain de zone) → multiplié au volume de l'utilisateur
//!   par [`BilanDeNiveau::gain_de_rendu`], raboté à l'unité, puis posé dans
//!   la [`super::ConsigneVolume`].

use super::rampe::{decibels, lineaire};
use crate::tess::chaine::{HEADROOM_AUTO_DB, HEADROOM_MANUEL_MIN_DB, Headroom};

/// rc14 (Didier) — la marge de sécurité ajoutée au gain maximal d'un filtre
/// de convolution : `max |H(f)|` est lu sur la grille de la FFT, et un pic
/// étroit peut tomber entre deux cases.
pub const MARGE_DE_SECURITE_CONVOLUTION_DB: f64 = 0.5;
/// Sous ce gain maximal, un filtre est tenu pour « à 0 dB » : un Dirac
/// exporté en 24 bits ou en `f32` ne pèse pas exactement 1.
pub const TOLERANCE_GAIN_CONVOLUTION_DB: f64 = 0.01;

/// rc14 (Didier) — ce que la convolution demande à la marge, et ce qui est
/// appliqué. Voir [`marge_de_convolution`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MargeDeConvolution {
    /// Le gain maximal du filtre, au pire canal, à la cadence du flux (dB).
    pub gain_max_db: f64,
    /// La marge qu'il faudrait pour qu'un sinus pleine échelle ne dépasse
    /// pas 0 dBFS : `−(gain_max + 0,5)`, bornée à [`HEADROOM_MANUEL_MIN_DB`] ;
    /// 0 pour un filtre qui n'amplifie pas.
    pub requise_db: f64,
    /// La marge que le CONVOLUEUR applique (≤ 0) : la requise en Auto ; en
    /// manuel, M quand aucun étage d'égaliseur ne la porte (sortie réseau
    /// sans égaliseur : personne d'autre ne l'appliquerait), 0 sinon (l'étage
    /// des égaliseurs pose déjà `min(M, L1)`, #9520 : jamais deux fois).
    pub appliquee_db: f64,
    /// Marge MANUELLE plus faible que la requise : le chemin du signal le
    /// dit, rien n'est changé. Avec un égaliseur empilé, c'est ce que sa
    /// réserve L1 laisse de M au filtre qui compte (`min(M, L1) − L1`).
    pub insuffisante: bool,
}

/// rc14 (Didier) — la marge d'un filtre de convolution, depuis le gain que
/// le convolueur DÉCLARE (`max |H(f)|`, comme le crossfeed déclare son gain
/// moyen). La SEULE règle : le convolueur l'applique, le chemin du signal la
/// publie, [`BilanDeNiveau`] la compte.
///
/// - Auto : `min(HEADROOM_AUTO_DB, −(gain_max + 0,5))`, bornée à −24 dB ;
///   un filtre qui n'amplifie pas (gain ≤ 0 dB) ne change rien.
/// - Manuel M, UNE seule autorité (#9546) : `laissee_par_l_egaliseur_db` est
///   ce que l'étage des égaliseurs, quand il tourne, laisse de M au filtre
///   (`min(M, L1) − L1`, ≤ 0 ; le gain fixe M sans bande laisse M). `None` :
///   aucun étage d'égaliseur ne tourne (sortie réseau sans égaliseur), et
///   c'est le convolueur qui porte M — sans quoi le filtre jouerait à 0 dB,
///   sans aucune protection. `insuffisante` si la marge qui reste au filtre
///   est au-dessus de la requise.
pub fn marge_de_convolution(
    headroom: &Headroom,
    gain_max_db: f64,
    laissee_par_l_egaliseur_db: Option<f64>,
) -> MargeDeConvolution {
    let gain = fini(gain_max_db);
    let requise_db = if gain > TOLERANCE_GAIN_CONVOLUTION_DB {
        (-(gain + MARGE_DE_SECURITE_CONVOLUTION_DB)).max(HEADROOM_MANUEL_MIN_DB)
    } else {
        0.0
    };
    match headroom {
        Headroom::Manual { db } => {
            let manuelle = fini(*db).min(0.0);
            let (appliquee_db, laissee) = match laissee_par_l_egaliseur_db {
                Some(l) => (0.0, fini(l).min(0.0)),
                None => (manuelle, manuelle),
            };
            MargeDeConvolution {
                gain_max_db: gain,
                requise_db,
                appliquee_db,
                insuffisante: requise_db < 0.0 && laissee > requise_db + 1e-9,
            }
        }
        Headroom::Auto {} | Headroom::Unknown => MargeDeConvolution {
            gain_max_db: gain,
            requise_db,
            appliquee_db: HEADROOM_AUTO_DB.min(requise_db).min(0.0),
            insuffisante: false,
        },
    }
}

/// Ce que le bilan prend en entrée (lu par l'hôte à chaque installation de
/// chaîne, de piste ou d'interrupteur ; jamais par bloc).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntreesNiveau {
    /// Le réglage `fixed.headroom` de la chaîne.
    pub headroom: Headroom,
    /// `Resolution::headroom_db` : 0 quand l'étape n'est pas effective
    /// (rien à protéger, blocage, valeur inconnue).
    pub headroom_resolu_db: f64,
    /// ReplayGain de la piste (préampli compris), en dB ; 0 s'il est éteint.
    pub replaygain_db: f64,
    /// `zone_{id}_gain_trim_db`.
    pub gain_de_zone_db: f64,
    /// Somme des gains MOYENS déclarés par les greffons EFFECTIFS (≤ 0 pour
    /// une réserve) ; 0 sans greffon.
    pub gain_moyen_greffons_db: f64,
    /// `zone_{id}_level_compensation` (vrai par défaut).
    pub compensation_active: bool,
    /// rc14 (Didier) — le gain MAXIMAL déclaré par le convolueur effectif
    /// (`max |H(f)|`, dB, pire canal, à la cadence du flux) ; 0 sans
    /// convolution. Voir [`marge_de_convolution`].
    pub gain_max_convolution_db: f64,
}

/// Le bilan. Voir le module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BilanDeNiveau {
    /// Headroom réellement appliqué par l'étage (≤ 0).
    pub headroom_db: f64,
    /// Part de l'automatique déjà couverte par la réserve des greffons (≥ 0).
    pub absorbe_par_les_greffons_db: f64,
    /// Gain de l'étage Headroom : headroom + part négative du niveau (≤ 0).
    pub gain_d_entree_db: f64,
    /// Ce que la compensation demande au rendu.
    pub compensation_db: f64,
    /// Compensation + part positive du niveau : ce qui multiplie le volume.
    pub gain_au_rendu_db: f64,
}

/// Le gain rendu par le volume, et ce que le rabot à l'unité en a mangé.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainDeRendu {
    /// Gain linéaire posé dans la consigne, dans `[0, 1]`.
    pub lineaire: f64,
    /// dB demandés au-delà de l'unité, donc non rendus (≥ 0) ; à publier,
    /// comme `unrendered_db` (#5069).
    pub rabote_db: f64,
}

fn fini(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

impl BilanDeNiveau {
    pub fn calculer(e: &EntreesNiveau) -> Self {
        let reserve = fini(e.gain_moyen_greffons_db).min(0.0);
        let resolu = fini(e.headroom_resolu_db).min(0.0);
        // rc14 (Didier) — la marge du filtre de convolution (comptée en Auto
        // seulement ; en manuel, le headroom résolu porte déjà M) : elle
        // protège un gain que la réserve des égaliseurs ne voit pas, elle
        // s'ajoute donc à la leur.
        let convolution =
            marge_de_convolution(&e.headroom, e.gain_max_convolution_db, None).appliquee_db;
        let (headroom_db, absorbe) = match e.headroom {
            Headroom::Auto {} if resolu < 0.0 => {
                // `resolu` vaut `HEADROOM_AUTO_DB` : la part que la réserve des
                // greffons ne couvre pas déjà.
                let reste = (resolu - reserve).min(0.0);
                (reste + convolution, reste - resolu)
            }
            Headroom::Auto {} => (resolu + convolution, 0.0),
            _ => (resolu, 0.0),
        };
        let niveau = fini(e.replaygain_db) + fini(e.gain_de_zone_db);
        let compensation_db = if e.compensation_active {
            -(headroom_db + fini(e.gain_moyen_greffons_db))
        } else {
            0.0
        };
        Self {
            headroom_db,
            absorbe_par_les_greffons_db: absorbe,
            gain_d_entree_db: headroom_db + niveau.min(0.0),
            compensation_db,
            gain_au_rendu_db: compensation_db + niveau.max(0.0),
        }
    }

    /// Le gain rendu pour un volume utilisateur `volume` (linéaire, 0..1) :
    /// `min(1, volume × 10^(gain_au_rendu_db/20))`. Jamais plus fort que la
    /// pleine échelle ; volume nul ⇒ 0 exact.
    pub fn gain_de_rendu(&self, volume: f64) -> GainDeRendu {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let demande = volume * lineaire(self.gain_au_rendu_db);
        if demande <= 1.0 {
            GainDeRendu {
                lineaire: demande,
                rabote_db: 0.0,
            }
        } else {
            GainDeRendu {
                lineaire: 1.0,
                rabote_db: decibels(demande),
            }
        }
    }
}
