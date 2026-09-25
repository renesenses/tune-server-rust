//! #2211 — **le fondu enchaîné qui superpose vraiment deux pistes.**
//!
//! # Ce que ce module remplace
//!
//! `playback::crossfade::CrossfadeHandler`, retiré en v0.9.146, baissait le
//! volume de l'`OutputTarget` à zéro puis remontait celui de la piste
//! suivante — dix commandes de volume par seconde, séquentiellement. Les deux
//! pistes n'étaient **jamais** décodées ni mélangées en même temps : entre
//! elles il n'y avait pas un recouvrement mais un **trou**, et sur une sortie
//! matérielle la rampe touchait le volume PERSISTANT de la zone. L'arbitrage
//! du 02/09/2026 est explicite : le volume matériel ne doit plus être touché,
//! le fondu se fait en mélangeant deux flux PCM décodés.
//!
//! Rien ici ne connaît un `OutputTarget`, un volume ni un périphérique. Ce
//! module ne voit que des mots `f32` entrelacés et un
//! [`PuitsDEchantillons`]. C'est structurel, pas une discipline : il n'a
//! aucun moyen d'atteindre un volume, et `tune-core/tests/
//! crossfade_pas_de_rampe_de_volume.rs` interdit que la rampe revienne.
//!
//! # Où il se branche
//!
//! [`BoucleProducteur`] (REF-7, #2219) est déjà générique : elle connaît un
//! `Read`, un étage et un puits, et son commentaire annonce depuis REF-7
//! qu'on pourra « lui brancher un second puits sans la toucher ». C'est
//! exactement ce que fait [`AtelierDeFondu`] : il rend **deux**
//! [`PuitsDEchantillons`] — [`VoieSortante`] et [`VoieEntrante`] — qui
//! partagent un seul moteur et un seul puits réel. Deux boucles producteur
//! inchangées, chacune avec son propre étage de conversion et son propre
//! décodeur, écrivent chacune dans sa voie ; le moteur mélange et livre.
//!
//! Deux producteurs, donc deux décodages **simultanés** : c'est la brique que
//! #2211 réclame et qui n'a jamais existé.
//!
//! [`BoucleProducteur`]: crate::outputs
//!
//! # Le mécanisme, en trois temps
//!
//! Le moteur ne sait pas d'avance où finit la piste sortante — un flux
//! décodé n'annonce pas sa dernière trame. Il **retient donc en permanence
//! les N dernières trames** de la sortante (`N` = la durée demandée) et
//! laisse passer tout le reste **inchangé**, mot pour mot. Quand la sortante
//! déclare sa fin ([`FonduEnchaine::fin_de_la_sortante`]), cette réserve
//! **est** la queue à superposer : la durée du recouvrement est exacte par
//! construction, jamais estimée.
//!
//! 1. **avant** : la sortante traverse sans être touchée (bit-perfect
//!    préservé), moins les N trames retenues ; l'entrante, si elle a déjà
//!    commencé à décoder, s'accumule ;
//! 2. **pendant** : trame par trame, `réserve[i] · g_sortant(i) +
//!    entrante[i] · g_entrant(i)`. Les deux sources sont dans le MÊME mot de
//!    sortie — c'est ce qu'aucune rampe de volume ne peut produire ;
//! 3. **après** : l'entrante traverse sans être touchée.
//!
//! # Continuité aux deux bornes
//!
//! Avec `t = i / (N - 1)`, `t` vaut exactement `0` à la première trame
//! mélangée et exactement `1` à la dernière. La première trame du
//! recouvrement est donc **exactement** la trame sortante qui l'aurait
//! précédée, et la dernière **exactement** la trame entrante qui la suit :
//! aucune marche à l'entrée ni à la sortie du fondu. Un `t = i / N` laisserait
//! `g_sortant = 1/N` sur la dernière trame — une discontinuité que l'oreille
//! entend comme un clic sur un fondu court.
//!
//! `N == 1` est le cas dégénéré : une seule trame, prise pure sortante. Il
//! est nommé plutôt que masqué.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::outputs::traits::{AudioSpec, FormatOuvert, PuitsDEchantillons, TransformationsReelles};

/// La courbe des deux enveloppes, **configurable** (critère 2 de #2211).
///
/// Les deux respectent `g_sortant(0) = 1`, `g_entrant(0) = 0`,
/// `g_sortant(1) = 0`, `g_entrant(1) = 1`. Elles diffèrent par ce qu'elles
/// font au MILIEU, et c'est tout l'enjeu :
///
/// * [`Self::Lineaire`] : à mi-course les deux gains valent `0,5`. Sur deux
///   sources décorrélées — deux pistes différentes, le cas normal — les
///   puissances s'additionnent et le niveau perçu **chute de 3 dB** au
///   milieu du fondu. C'est le creux que l'oreille entend ;
/// * [`Self::PuissanceConstante`] : `cos` et `sin` d'un quart de tour. À
///   mi-course les deux gains valent `√2/2 ≈ 0,707`, dont les carrés
///   s'additionnent à `1` — la puissance est constante d'un bout à l'autre.
///   C'est le défaut, et c'est le minimum que #2211 exige.
///
/// Le linéaire n'est pas gardé par nostalgie : sur deux sources
/// **corrélées** — la même note tenue de part et d'autre d'une frontière de
/// piste, un enregistrement live découpé — ce sont les amplitudes qui
/// s'additionnent, et c'est le linéaire qui tient le niveau tandis que la
/// puissance constante fait une bosse de +3 dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CourbeDeFondu {
    /// Amplitudes complémentaires : `1 - t` et `t`.
    Lineaire,
    /// Puissance constante : `cos(t·π/2)` et `sin(t·π/2)`.
    #[default]
    PuissanceConstante,
}

impl CourbeDeFondu {
    /// Les deux gains à l'avancement `t ∈ [0, 1]` : `(sortant, entrant)`.
    ///
    /// `t` est **saturé** dans `[0, 1]` : hors de cet intervalle la question
    /// n'a pas de sens, et un gain négatif inverserait la phase d'une des
    /// deux sources — un défaut qui ne s'entend pas sur un test de gain et
    /// s'entend très bien sur de la musique.
    #[must_use]
    pub fn gains(self, t: f32) -> (f32, f32) {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Lineaire => (1.0 - t, t),
            Self::PuissanceConstante => {
                let angle = t * std::f32::consts::FRAC_PI_2;
                (angle.cos(), angle.sin())
            }
        }
    }
}

/// Ce que le moteur a fait des mots qu'on lui a donnés.
///
/// Un seul motif d'échec, et il porte le même sens que celui de
/// [`PuitsDEchantillons::ecrire`] : le puits réel a cessé de consommer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtatDuFondu {
    /// Le puits a tout pris.
    Livre,
    /// Le puits a cessé de consommer : le producteur doit se démonter.
    PuitsMort,
}

impl EtatDuFondu {
    /// `true` tant que le puits consomme — la valeur que rend
    /// [`PuitsDEchantillons::ecrire`].
    #[must_use]
    pub const fn vivant(self) -> bool {
        matches!(self, Self::Livre)
    }

    const fn depuis(vivant: bool) -> Self {
        if vivant { Self::Livre } else { Self::PuitsMort }
    }
}

/// Où en est la transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseDuFondu {
    /// La sortante joue seule ; ses N dernières trames sont en réserve.
    Sortante,
    /// Les deux sources sont mélangées dans le même mot.
    Recouvrement,
    /// L'entrante joue seule.
    Entrante,
}

/// Le moteur du fondu enchaîné : **deux flux `f32` entrelacés, un puits**.
///
/// Mono-fil et sans verrou. Le partage entre deux producteurs est le rôle
/// d'[`AtelierDeFondu`], qui n'ajoute qu'un `Mutex` par-dessus.
///
/// Tous les mots sont au format **ouvert** — celui du périphérique, celui du
/// puits — c'est-à-dire après adaptation de canaux et rééchantillonnage. Deux
/// pistes de cadences différentes se mélangent donc sans précaution
/// particulière : elles sont déjà à la cadence du périphérique quand elles
/// arrivent ici. Mélanger avant la conversion aurait exigé de rééchantillonner
/// l'une des deux au vol, et c'est précisément le nœud que la frontière
/// producteur/puits de R1 dénoue.
pub struct FonduEnchaine {
    format: FormatOuvert,
    /// Trames de recouvrement demandées. Jamais nul.
    recouvrement: usize,
    courbe: CourbeDeFondu,
    /// La queue de la sortante, retenue : au plus `recouvrement` trames.
    reserve: VecDeque<f32>,
    /// Les mots de l'entrante pas encore consommés.
    entrante: VecDeque<f32>,
    sortante_finie: bool,
    /// Le recouvrement RÉELLEMENT retenu à l'instant de la fin, en trames.
    /// Plus court que `recouvrement` si la sortante était plus courte qu'un
    /// recouvrement — un jingle de deux secondes sous un fondu de cinq.
    recouvrement_effectif: usize,
    /// Trames de recouvrement déjà livrées.
    trames_melangees: usize,
    fondu_termine: bool,
    /// Mots de la sortante entrés dans le moteur.
    mots_sortants_recus: u64,
    /// Mots de l'entrante entrés dans le moteur.
    mots_entrants_recus: u64,
    /// Mots livrés au puits réel.
    mots_livres: u64,
}

impl FonduEnchaine {
    /// Monte un fondu de `trames_de_recouvrement` trames sur ce format.
    ///
    /// `None` quand le recouvrement est nul : un fondu de zéro trame n'est
    /// pas un fondu court, c'est une absence de fondu, et l'appelant doit
    /// alors brancher le puits réel directement plutôt que de traverser un
    /// moteur qui ne ferait rien. Zéro canal est déjà impossible —
    /// [`FormatOuvert`] le porte, mais c'est [`AudioSpec::nouvelle`] qui
    /// refuse — donc il est refusé ici aussi, explicitement : c'est un
    /// diviseur.
    #[must_use]
    pub fn nouveau(
        format: FormatOuvert,
        trames_de_recouvrement: usize,
        courbe: CourbeDeFondu,
    ) -> Option<Self> {
        if trames_de_recouvrement == 0 || format.canaux == 0 {
            return None;
        }
        Some(Self {
            format,
            recouvrement: trames_de_recouvrement,
            courbe,
            reserve: VecDeque::new(),
            entrante: VecDeque::new(),
            sortante_finie: false,
            recouvrement_effectif: 0,
            trames_melangees: 0,
            fondu_termine: false,
            mots_sortants_recus: 0,
            mots_entrants_recus: 0,
            mots_livres: 0,
        })
    }

    /// Le même, depuis une durée. `None` aux mêmes conditions, plus une
    /// durée qui ne fait pas une trame entière à cette cadence.
    #[must_use]
    pub fn pendant(
        format: FormatOuvert,
        duree: std::time::Duration,
        courbe: CourbeDeFondu,
    ) -> Option<Self> {
        let trames = (duree.as_secs_f64() * f64::from(format.cadence)).round();
        if !trames.is_finite() || trames < 1.0 || trames > usize::MAX as f64 {
            return None;
        }
        Self::nouveau(format, trames as usize, courbe)
    }

    /// Le format des mots que ce moteur reçoit et livre.
    #[must_use]
    pub const fn format(&self) -> FormatOuvert {
        self.format
    }

    /// La courbe posée à la construction.
    #[must_use]
    pub const fn courbe(&self) -> CourbeDeFondu {
        self.courbe
    }

    /// Trames de recouvrement demandées.
    #[must_use]
    pub const fn trames_de_recouvrement(&self) -> usize {
        self.recouvrement
    }

    /// Trames de recouvrement **réellement** retenues, connues seulement
    /// après [`Self::fin_de_la_sortante`]. Zéro avant.
    #[must_use]
    pub const fn recouvrement_effectif(&self) -> usize {
        self.recouvrement_effectif
    }

    /// Trames déjà mélangées, c'est-à-dire livrées avec les deux sources
    /// dans le même mot.
    #[must_use]
    pub const fn trames_melangees(&self) -> usize {
        self.trames_melangees
    }

    /// Où en est la transition.
    #[must_use]
    pub const fn phase(&self) -> PhaseDuFondu {
        if self.fondu_termine {
            PhaseDuFondu::Entrante
        } else if self.sortante_finie {
            PhaseDuFondu::Recouvrement
        } else {
            PhaseDuFondu::Sortante
        }
    }

    /// Le moteur touche-t-il aux échantillons **en ce moment** ?
    ///
    /// C'est la réponse au critère 4 de #2211 : déclarer le traitement dans
    /// le chemin du signal et retirer le bit-perfect. Hors du recouvrement,
    /// les mots traversent inchangés et le bit-perfect reste entier ; dans le
    /// recouvrement, deux sources sont additionnées sous enveloppe et il n'y
    /// a plus rien de bit-perfect à déclarer.
    #[must_use]
    pub const fn traitement_actif(&self) -> bool {
        matches!(self.phase(), PhaseDuFondu::Recouvrement)
    }

    /// Ce que le moteur fait réellement au signal, à cet instant, pour le
    /// créneau que `LocalOutput` publie (REF-6b, #3987).
    ///
    /// `dsp_actif` est vrai pendant le recouvrement **et lui seul** : c'est
    /// ce qui éteint le statut bit-perfect au bon moment plutôt que pour
    /// toute la piste.
    #[must_use]
    pub fn transformations(&self, entree: AudioSpec) -> TransformationsReelles {
        TransformationsReelles::nouvelles(entree, self.format, self.traitement_actif())
    }

    /// Mots reçus de la sortante, de l'entrante, et livrés au puits.
    ///
    /// Publié pour les témoins : `livres == sortants + entrants -
    /// recouvrement_effectif · canaux` est l'égalité de conservation du
    /// fondu, et c'est elle qui dit qu'aucun échantillon n'a été perdu ni
    /// dupliqué.
    #[must_use]
    pub const fn compteurs(&self) -> (u64, u64, u64) {
        (
            self.mots_sortants_recus,
            self.mots_entrants_recus,
            self.mots_livres,
        )
    }

    fn canaux(&self) -> usize {
        self.format.canaux as usize
    }

    /// Livre `mots` au puits réel et compte.
    fn livrer(&mut self, puits: &mut (dyn PuitsDEchantillons + '_), mots: &[f32]) -> bool {
        if mots.is_empty() {
            return true;
        }
        self.mots_livres += mots.len() as u64;
        puits.ecrire(mots)
    }

    /// Des mots de la piste **sortante**.
    ///
    /// Tout traverse inchangé sauf les `recouvrement` dernières trames, qui
    /// restent en réserve. Après [`Self::fin_de_la_sortante`], plus rien
    /// n'est accepté : la piste est finie, et accepter en silence des mots
    /// qui arrivent trop tard ferait un fondu sur un signal déjà consommé.
    /// Ils sont ignorés, et le compteur ne bouge pas.
    pub fn pousser_sortante(
        &mut self,
        puits: &mut (dyn PuitsDEchantillons + '_),
        mots: &[f32],
    ) -> EtatDuFondu {
        if self.sortante_finie {
            return EtatDuFondu::Livre;
        }
        self.mots_sortants_recus += mots.len() as u64;
        self.reserve.extend(mots.iter().copied());

        let plafond = self.recouvrement * self.canaux();
        if self.reserve.len() <= plafond {
            return EtatDuFondu::Livre;
        }
        // Ce qui dépasse la réserve part tel quel — mot pour mot, aucune
        // enveloppe : hors du recouvrement le fondu ne touche à rien.
        let a_livrer = self.reserve.len() - plafond;
        let sortie: Vec<f32> = self.reserve.drain(..a_livrer).collect();
        EtatDuFondu::depuis(self.livrer(puits, &sortie))
    }

    /// Des mots de la piste **entrante**.
    ///
    /// Avant la fin de la sortante ils s'accumulent : c'est normal et c'est
    /// même le but — l'entrante doit avoir décodé de quoi remplir le
    /// recouvrement à l'instant où la sortante s'arrête, sinon il n'y a
    /// personne pour la seconde moitié du mélange. Après, ils sont
    /// consommés au fil de l'eau.
    pub fn pousser_entrante(
        &mut self,
        puits: &mut (dyn PuitsDEchantillons + '_),
        mots: &[f32],
    ) -> EtatDuFondu {
        self.mots_entrants_recus += mots.len() as u64;
        self.entrante.extend(mots.iter().copied());
        self.avancer(puits)
    }

    /// La piste sortante n'a plus rien à donner : ce qui est en réserve EST
    /// la queue à superposer.
    ///
    /// Appelable une seule fois ; les appels suivants ne font rien. La durée
    /// du recouvrement est figée ici, et c'est la seule chose qui la fixe.
    pub fn fin_de_la_sortante(&mut self, puits: &mut (dyn PuitsDEchantillons + '_)) -> EtatDuFondu {
        if self.sortante_finie {
            return EtatDuFondu::Livre;
        }
        self.sortante_finie = true;
        self.recouvrement_effectif = self.reserve.len() / self.canaux();
        if self.recouvrement_effectif == 0 {
            // La sortante n'a pas rendu une seule trame complète : il n'y a
            // rien à superposer, et l'entrante prend la suite directement.
            self.reserve.clear();
            self.fondu_termine = true;
        }
        self.avancer(puits)
    }

    /// Le gain de la trame `i` du recouvrement.
    ///
    /// `t = i / (N - 1)` : `0` exactement sur la première trame, `1`
    /// exactement sur la dernière. Voir l'en-tête du module pour ce que cette
    /// borne achète.
    fn gains_de_la_trame(&self, i: usize) -> (f32, f32) {
        if self.recouvrement_effectif <= 1 {
            return self.courbe.gains(0.0);
        }
        let t = i as f32 / (self.recouvrement_effectif - 1) as f32;
        self.courbe.gains(t)
    }

    /// Mélange et livre tout ce qui peut l'être, puis laisse passer
    /// l'entrante une fois le recouvrement terminé.
    fn avancer(&mut self, puits: &mut (dyn PuitsDEchantillons + '_)) -> EtatDuFondu {
        if !self.sortante_finie {
            return EtatDuFondu::Livre;
        }
        let canaux = self.canaux();

        if !self.fondu_termine {
            // Autant de trames que les DEUX sources peuvent en fournir.
            let disponibles = (self.reserve.len() / canaux).min(self.entrante.len() / canaux);
            if disponibles > 0 {
                let mut melange = Vec::with_capacity(disponibles * canaux);
                for _ in 0..disponibles {
                    let (g_sortant, g_entrant) = self.gains_de_la_trame(self.trames_melangees);
                    for _ in 0..canaux {
                        let s = self.reserve.pop_front().unwrap_or(0.0);
                        let e = self.entrante.pop_front().unwrap_or(0.0);
                        melange.push(s * g_sortant + e * g_entrant);
                    }
                    self.trames_melangees += 1;
                }
                if self.trames_melangees >= self.recouvrement_effectif {
                    self.fondu_termine = true;
                }
                if !self.livrer(puits, &melange) {
                    return EtatDuFondu::PuitsMort;
                }
            }
        }

        if self.fondu_termine && !self.entrante.is_empty() {
            let suite: Vec<f32> = self.entrante.drain(..).collect();
            return EtatDuFondu::depuis(self.livrer(puits, &suite));
        }
        EtatDuFondu::Livre
    }

    /// Fin de la transition : sort tout ce qui reste.
    ///
    /// À appeler quand l'entrante n'a plus rien à donner non plus. Si elle
    /// s'est tarie AVANT la fin du recouvrement — une piste plus courte que
    /// le fondu — le reste de la réserve part quand même, avec son enveloppe
    /// poursuivie : la sortante finit son extinction au lieu d'être coupée
    /// net. Perdre ces trames serait un trou, exactement celui que #2211
    /// dénonce.
    pub fn vider(&mut self, puits: &mut (dyn PuitsDEchantillons + '_)) -> EtatDuFondu {
        if !self.sortante_finie && self.fin_de_la_sortante(puits) == EtatDuFondu::PuitsMort {
            return EtatDuFondu::PuitsMort;
        }
        if self.avancer(puits) == EtatDuFondu::PuitsMort {
            return EtatDuFondu::PuitsMort;
        }

        let canaux = self.canaux();
        if !self.fondu_termine && self.reserve.len() >= canaux {
            let trames = self.reserve.len() / canaux;
            let mut queue = Vec::with_capacity(trames * canaux);
            for _ in 0..trames {
                let (g_sortant, _) = self.gains_de_la_trame(self.trames_melangees);
                for _ in 0..canaux {
                    queue.push(self.reserve.pop_front().unwrap_or(0.0) * g_sortant);
                }
                self.trames_melangees += 1;
            }
            self.fondu_termine = true;
            if !self.livrer(puits, &queue) {
                return EtatDuFondu::PuitsMort;
            }
        }
        self.reserve.clear();
        self.fondu_termine = true;

        if self.entrante.is_empty() {
            return EtatDuFondu::Livre;
        }
        let suite: Vec<f32> = self.entrante.drain(..).collect();
        EtatDuFondu::depuis(self.livrer(puits, &suite))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Le partage entre DEUX producteurs.
// ───────────────────────────────────────────────────────────────────────────

/// L'état que les deux voies partagent : le moteur et le puits réel.
struct EtatPartage<'p> {
    moteur: FonduEnchaine,
    puits: Box<dyn PuitsDEchantillons + Send + 'p>,
}

/// Deux [`PuitsDEchantillons`] pour un seul puits réel.
///
/// C'est la pièce qui permet de brancher **deux boucles producteur
/// inchangées** — donc deux décodeurs, donc deux décodages simultanés — sur
/// une seule sortie. Chaque voie prend le verrou le temps d'un bloc ; le
/// producteur qui attend est celui qui a de l'avance, et c'est le
/// comportement voulu.
///
/// # Le verrou et le puits qui bloque
///
/// `ecrire` sur le puits réel peut bloquer — l'anneau cpal attend que le
/// rappel draine. Ce blocage a lieu **sous le verrou**, donc l'autre voie
/// attend aussi. Ce n'est pas un défaut : les deux flux doivent avancer à la
/// vitesse du DAC, pas à celle du décodeur le plus rapide. Sans cela
/// l'entrante décoderait la piste entière en mémoire pendant que la sortante
/// finit.
///
/// # Empoisonnement
///
/// Un producteur qui panique empoisonne le verrou. Les deux voies rendent
/// alors `false` — « le puits a cessé de consommer » — et les boucles
/// productrices se démontent, ce qui est exactement le bon geste : mieux vaut
/// deux fils qui s'arrêtent qu'un fondu à moitié écrit dans le DAC.
pub struct AtelierDeFondu<'p> {
    partage: Arc<Mutex<EtatPartage<'p>>>,
}

impl<'p> AtelierDeFondu<'p> {
    /// Monte l'atelier sur un moteur et le puits réel.
    #[must_use]
    pub fn nouveau(moteur: FonduEnchaine, puits: Box<dyn PuitsDEchantillons + Send + 'p>) -> Self {
        Self {
            partage: Arc::new(Mutex::new(EtatPartage { moteur, puits })),
        }
    }

    /// La voie de la piste qui se termine.
    #[must_use]
    pub fn voie_sortante(&self) -> VoieSortante<'p> {
        VoieSortante {
            partage: Arc::clone(&self.partage),
        }
    }

    /// La voie de la piste qui commence.
    #[must_use]
    pub fn voie_entrante(&self) -> VoieEntrante<'p> {
        VoieEntrante {
            partage: Arc::clone(&self.partage),
        }
    }

    /// Déclare la fin de la piste sortante. À appeler quand sa boucle
    /// producteur rend `FinDeFlux`.
    pub fn fin_de_la_sortante(&self) -> EtatDuFondu {
        self.avec(|etat| etat.moteur.fin_de_la_sortante(etat.puits.as_mut()))
    }

    /// Sort tout ce qui reste. À appeler quand les deux boucles ont rendu.
    pub fn vider(&self) -> EtatDuFondu {
        self.avec(|etat| etat.moteur.vider(etat.puits.as_mut()))
    }

    /// Une photo des compteurs du moteur : `(sortants, entrants, livrés)`.
    #[must_use]
    pub fn compteurs(&self) -> (u64, u64, u64) {
        self.partage
            .lock()
            .map(|etat| etat.moteur.compteurs())
            .unwrap_or((0, 0, 0))
    }

    /// Trames réellement superposées.
    #[must_use]
    pub fn trames_melangees(&self) -> usize {
        self.partage
            .lock()
            .map(|etat| etat.moteur.trames_melangees())
            .unwrap_or(0)
    }

    /// Le moteur touche-t-il aux échantillons en ce moment ?
    #[must_use]
    pub fn traitement_actif(&self) -> bool {
        self.partage
            .lock()
            .map(|etat| etat.moteur.traitement_actif())
            .unwrap_or(false)
    }

    fn avec(&self, geste: impl FnOnce(&mut EtatPartage<'p>) -> EtatDuFondu) -> EtatDuFondu {
        match self.partage.lock() {
            Ok(mut etat) => geste(&mut etat),
            Err(_) => EtatDuFondu::PuitsMort,
        }
    }
}

/// Le puits de la piste qui se termine. Se branche à une boucle producteur
/// telle quelle.
pub struct VoieSortante<'p> {
    partage: Arc<Mutex<EtatPartage<'p>>>,
}

/// Le puits de la piste qui commence. Se branche à une boucle producteur
/// telle quelle.
pub struct VoieEntrante<'p> {
    partage: Arc<Mutex<EtatPartage<'p>>>,
}

impl PuitsDEchantillons for VoieSortante<'_> {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        match self.partage.lock() {
            Ok(mut etat) => {
                let EtatPartage { moteur, puits } = &mut *etat;
                moteur.pousser_sortante(puits.as_mut(), mots).vivant()
            }
            Err(_) => false,
        }
    }
}

impl PuitsDEchantillons for VoieEntrante<'_> {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        match self.partage.lock() {
            Ok(mut etat) => {
                let EtatPartage { moteur, puits } = &mut *etat;
                moteur.pousser_entrante(puits.as_mut(), mots).vivant()
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un puits de test qui retient tout ce qu'on lui donne.
    #[derive(Default)]
    struct PuitsMemoire {
        mots: Vec<f32>,
        vivant: bool,
    }

    impl PuitsMemoire {
        fn nouveau() -> Self {
            Self {
                mots: Vec::new(),
                vivant: true,
            }
        }
    }

    impl PuitsDEchantillons for PuitsMemoire {
        fn ecrire(&mut self, mots: &[f32]) -> bool {
            self.mots.extend_from_slice(mots);
            self.vivant
        }
    }

    fn stereo(cadence: u32) -> FormatOuvert {
        FormatOuvert::new(cadence, 2)
    }

    #[test]
    fn un_recouvrement_nul_est_refuse() {
        assert!(FonduEnchaine::nouveau(stereo(44_100), 0, CourbeDeFondu::Lineaire).is_none());
    }

    #[test]
    fn la_puissance_constante_tient_la_puissance_au_milieu() {
        let (s, e) = CourbeDeFondu::PuissanceConstante.gains(0.5);
        assert!((s * s + e * e - 1.0).abs() < 1e-6, "s={s} e={e}");
        let (s, e) = CourbeDeFondu::Lineaire.gains(0.5);
        assert!((s * s + e * e - 0.5).abs() < 1e-6, "s={s} e={e}");
    }

    #[test]
    fn les_deux_courbes_sont_exactes_aux_bornes() {
        for courbe in [CourbeDeFondu::Lineaire, CourbeDeFondu::PuissanceConstante] {
            let (s0, e0) = courbe.gains(0.0);
            let (s1, e1) = courbe.gains(1.0);
            assert!(
                (s0 - 1.0).abs() < 1e-6 && e0.abs() < 1e-6,
                "{courbe:?} en 0"
            );
            assert!(
                s1.abs() < 1e-6 && (e1 - 1.0).abs() < 1e-6,
                "{courbe:?} en 1"
            );
        }
    }

    #[test]
    fn hors_du_recouvrement_les_mots_traversent_inchanges() {
        let mut puits = PuitsMemoire::nouveau();
        let mut moteur = FonduEnchaine::nouveau(stereo(48_000), 4, CourbeDeFondu::Lineaire)
            .expect("recouvrement non nul");
        // 10 trames stéréo, valeurs distinctes.
        let mots: Vec<f32> = (0..20).map(|i| i as f32).collect();
        assert_eq!(
            moteur.pousser_sortante(&mut puits, &mots),
            EtatDuFondu::Livre
        );
        // 4 trames retenues → 6 trames livrées, mot pour mot.
        assert_eq!(puits.mots, mots[..12]);
    }

    #[test]
    fn le_recouvrement_fait_exactement_la_duree_demandee() {
        let mut puits = PuitsMemoire::nouveau();
        let mut moteur = FonduEnchaine::nouveau(stereo(48_000), 5, CourbeDeFondu::Lineaire)
            .expect("recouvrement non nul");
        moteur.pousser_sortante(&mut puits, &vec![1.0; 100 * 2]);
        moteur.pousser_entrante(&mut puits, &vec![2.0; 100 * 2]);
        moteur.fin_de_la_sortante(&mut puits);
        moteur.vider(&mut puits);
        assert_eq!(moteur.recouvrement_effectif(), 5);
        assert_eq!(moteur.trames_melangees(), 5);
        // Conservation : livrés = sortants + entrants − recouvrement·canaux.
        let (s, e, l) = moteur.compteurs();
        assert_eq!(l, s + e - 5 * 2);
    }

    #[test]
    fn une_sortante_plus_courte_que_le_fondu_raccourcit_le_recouvrement() {
        let mut puits = PuitsMemoire::nouveau();
        let mut moteur = FonduEnchaine::nouveau(stereo(48_000), 64, CourbeDeFondu::Lineaire)
            .expect("recouvrement non nul");
        moteur.pousser_sortante(&mut puits, &[1.0; 10 * 2]);
        moteur.pousser_entrante(&mut puits, &vec![2.0; 40 * 2]);
        moteur.fin_de_la_sortante(&mut puits);
        moteur.vider(&mut puits);
        assert_eq!(moteur.recouvrement_effectif(), 10);
        assert_eq!(moteur.trames_melangees(), 10);
    }

    #[test]
    fn le_traitement_n_est_declare_que_pendant_le_recouvrement() {
        let mut puits = PuitsMemoire::nouveau();
        let mut moteur = FonduEnchaine::nouveau(stereo(48_000), 8, CourbeDeFondu::Lineaire)
            .expect("recouvrement non nul");
        assert!(!moteur.traitement_actif());
        moteur.pousser_sortante(&mut puits, &vec![1.0; 32 * 2]);
        assert!(!moteur.traitement_actif());
        moteur.fin_de_la_sortante(&mut puits);
        assert!(moteur.traitement_actif());
        moteur.pousser_entrante(&mut puits, &[2.0; 8 * 2]);
        assert!(!moteur.traitement_actif());
    }

    #[test]
    fn un_puits_mort_se_dit_et_ne_se_confond_pas_avec_un_arret() {
        let mut puits = PuitsMemoire::nouveau();
        puits.vivant = false;
        let mut moteur = FonduEnchaine::nouveau(stereo(48_000), 2, CourbeDeFondu::Lineaire)
            .expect("recouvrement non nul");
        assert_eq!(
            moteur.pousser_sortante(&mut puits, &[1.0; 10 * 2]),
            EtatDuFondu::PuitsMort
        );
    }
}
