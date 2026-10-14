//! #6132 — la sortie double : la même écoute sur deux périphériques locaux,
//! gardés synchrones malgré leurs deux horloges.
//!
//! Deux cartes son ont deux quartz. À 100 ppm d'écart — l'ordre de grandeur
//! courant — deux flux ouverts séparément s'éloignent de 6 ms par minute :
//! un écho audible au bout de quelques minutes, un décrochage au bout d'une
//! heure. Rien dans la chaîne locale ne le traitait (seul OAAT horodate).
//!
//! Le principe est celui d'un pont d'horloges (`zita-ajbridge`, `alsa_out`) :
//!
//! - le périphérique **A** est le maître. Son rappel de rendu donne la
//!   cadence ; ce qu'il joue est recopié, période par période, vers **B** ;
//! - la recopie passe par un **rééchantillonneur fin** (sinc fenêtré,
//!   polyphasé) dont le rapport vaut `cadence B / cadence A` corrigé de
//!   quelques ppm ;
//! - la correction est **pilotée par l'écart mesuré** : le remplissage de
//!   l'anneau de B. Si B consomme plus vite que A ne produit, l'anneau se
//!   vide et le rapport monte ; s'il consomme moins vite, l'anneau se remplit
//!   et le rapport baisse. Un régulateur proportionnel-intégral ramène le
//!   remplissage à sa consigne, donc l'écart entre A et B à une constante ;
//! - A est retardé, par une ligne à retard, de la même consigne : ce que A
//!   joue à un instant est ce que B joue au même instant, au remplissage
//!   près de l'anneau de B (une demi-période de B en moyenne).
//!
//! Tout ce qui suit tourne dans le rappel temps réel de A : aucune allocation
//! après la construction, aucun verrou bloquant. Ce module ne dépend ni de
//! cpal ni de la feature `local-audio` : la règle se juge par `cargo test`
//! sous Linux, sur deux périphériques simulés (`sortie_double_tests`).

/// Nombre de coefficients du filtre d'interpolation (pair). 64 : bande
/// passante plate jusqu'à ~0,9 × Nyquist, réjection au-delà de 90 dB.
pub const COEFFICIENTS: usize = 64;
/// Nombre de phases tabulées entre deux échantillons d'entrée ; la phase
/// réelle est interpolée linéairement entre deux phases voisines.
const PHASES: usize = 256;
/// Bande passante du filtre, en fraction de la fréquence de Nyquist la plus
/// basse des deux cadences.
const BANDE_PASSANTE: f64 = 0.92;

/// La correction ne sort jamais de ±2 000 ppm : au-delà, ce n'est plus une
/// dérive de quartz, c'est un périphérique qui ne tient pas sa cadence (ou
/// une cadence nominale mal lue), et corriger plus fort ferait entendre la
/// correction.
pub const CORRECTION_MAX: f64 = 2.0e-3;
/// Pulsation propre de la boucle, en rad/s. 0,1 : la correction suit une
/// dérive en une vingtaine de secondes, sans moduler la hauteur sur le
/// grain d'une période.
const PULSATION_PROPRE: f64 = 0.1;
/// Constante de temps du filtre passe-bas de la mesure, en secondes. Le
/// remplissage de l'anneau de B est une dent de scie de la taille d'une
/// période de B ; c'est sa moyenne qu'on régule.
const LISSAGE_S: f64 = 1.0;

/// Consigne de remplissage par défaut de l'anneau de B, en secondes : de quoi
/// couvrir plusieurs périodes de B (10 ms en WASAPI partagé) et les à-coups
/// d'ordonnancement, sans retarder le départ de façon perceptible.
pub const CONSIGNE_PAR_DEFAUT_S: f64 = 0.040;

/// L'anneau qui alimente le périphérique B, vu du recopieur.
///
/// En production c'est le `RingBuf` SPSC de la sortie locale (producteur : le
/// rappel de A ; consommateur : le rappel de B). Dans les tests, un anneau
/// simulé.
pub trait AnneauSecondaire {
    /// Pousse des échantillons entrelacés ; rend le nombre accepté.
    fn pousser(&self, echantillons: &[f32]) -> usize;
    /// Échantillons entrelacés en attente de lecture par B.
    fn en_attente(&self) -> usize;
    /// Taille, en échantillons entrelacés, de la dernière période que le
    /// rappel de B a lue ; 0 tant qu'il n'a rien lu.
    fn periode_du_lecteur(&self) -> usize {
        0
    }
    /// Secondes écoulées depuis la dernière lecture de B (même horloge pour
    /// les deux rappels).
    ///
    /// B lit une période d'un coup : le remplissage de l'anneau est une dent
    /// de scie, et le lire à l'instant où A passe donne une valeur qui dépend
    /// de la PHASE entre les deux rappels. Deux cartes aux périodes égales
    /// (10 ms en WASAPI partagé, le cas courant) gardent la même phase pendant
    /// des minutes : la régulation se calait alors jusqu'à une demi-période à
    /// côté. Avec cet instant, le recopieur reconstitue ce que B fait entendre
    /// en continu : `en attente + période lue − écoulé × cadence`.
    fn depuis_la_derniere_lecture_s(&self) -> Option<f64> {
        None
    }
    /// Latence matérielle de B, en secondes, telle que son dernier rappel l'a
    /// lue dans les horodatages du pilote (instant de lecture − instant du
    /// rappel). `None` : le pilote ne la donne pas.
    fn latence_du_lecteur_s(&self) -> Option<f64> {
        None
    }
    /// Le recopieur publie ici, à chaque période, l'avance que B doit prendre
    /// sur sa consigne nominale du côté de A (latence de A + décalage manuel),
    /// pour que le rappel de B parte au bon seuil avant même la première
    /// mesure.
    fn publier_l_avance_de_a(&self, _avance_s: f64) {}
    /// Taille, en échantillons entrelacés, de la période que le rappel de B
    /// demande, connue dès son premier rappel — avant même qu'il lise. Sert à
    /// régler le retard commun avant que le son ne sorte.
    fn periode_annoncee(&self) -> usize {
        self.periode_du_lecteur()
    }
}

macro_rules! deleguer_l_anneau {
    ($pointeur:ty) => {
        impl<T: AnneauSecondaire + ?Sized> AnneauSecondaire for $pointeur {
            fn pousser(&self, echantillons: &[f32]) -> usize {
                (**self).pousser(echantillons)
            }
            fn en_attente(&self) -> usize {
                (**self).en_attente()
            }
            fn periode_du_lecteur(&self) -> usize {
                (**self).periode_du_lecteur()
            }
            fn depuis_la_derniere_lecture_s(&self) -> Option<f64> {
                (**self).depuis_la_derniere_lecture_s()
            }
            fn latence_du_lecteur_s(&self) -> Option<f64> {
                (**self).latence_du_lecteur_s()
            }
            fn publier_l_avance_de_a(&self, avance_s: f64) {
                (**self).publier_l_avance_de_a(avance_s)
            }
            fn periode_annoncee(&self) -> usize {
                (**self).periode_annoncee()
            }
        }
    };
}
deleguer_l_anneau!(std::sync::Arc<T>);
deleguer_l_anneau!(std::rc::Rc<T>);

/// Table polyphasée d'un sinc fenêtré (Blackman-Harris), chaque phase
/// normalisée à un gain unitaire en continu.
struct TableSinc {
    /// `(PHASES + 1) × COEFFICIENTS` : la phase `PHASES` est la phase 0
    /// décalée d'un échantillon, pour interpoler sans cas particulier.
    coefficients: Box<[f32]>,
}

impl TableSinc {
    fn nouvelle(coupure: f64) -> Self {
        let mut coefficients = vec![0f32; (PHASES + 1) * COEFFICIENTS];
        let demi = (COEFFICIENTS / 2) as f64;
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let ligne = &mut coefficients[p * COEFFICIENTS..(p + 1) * COEFFICIENTS];
            let mut somme = 0.0f64;
            let mut valeurs = [0f64; COEFFICIENTS];
            for (k, v) in valeurs.iter_mut().enumerate() {
                // Coefficient k pondère l'entrée `i - (demi - 1) + k` pour une
                // sortie à la position `i + frac`.
                let t = k as f64 - (demi - 1.0) - frac;
                let sinc = if t.abs() < 1e-12 {
                    1.0
                } else {
                    let x = std::f64::consts::PI * coupure * t;
                    x.sin() / x
                };
                // Fenêtre de Blackman-Harris centrée, largeur `COEFFICIENTS`.
                let n = (t + demi) / (2.0 * demi);
                let w = if (0.0..=1.0).contains(&n) {
                    let a = 2.0 * std::f64::consts::PI * n;
                    0.35875 - 0.48829 * a.cos() + 0.14128 * (2.0 * a).cos()
                        - 0.01168 * (3.0 * a).cos()
                } else {
                    0.0
                };
                *v = coupure * sinc * w;
                somme += *v;
            }
            for (c, v) in ligne.iter_mut().zip(valeurs.iter()) {
                *c = (*v / somme) as f32;
            }
        }
        Self {
            coefficients: coefficients.into_boxed_slice(),
        }
    }
}

/// Régulateur proportionnel-intégral de l'écart, en secondes.
///
/// Le procédé est un intégrateur pur : `d(écart)/dt = dérive + correction`.
/// Avec `kp = 2ζω` et `ki = ω²` (ζ = 1), la boucle est amortie critique ;
/// l'intégrale absorbe la dérive constante, l'erreur statique est nulle.
///
/// Deux régimes. Au départ de B, l'écart initial vaut jusqu'à une période de
/// A (B ne sait pas où en est A dans sa période quand il démarre) : la boucle
/// tourne vite ([`PULSATION_ACCROCHAGE`]) pendant [`DUREE_ACCROCHAGE_S`],
/// puis lentement ([`PULSATION_PROPRE`]) pour ne plus suivre que la dérive
/// des quartz, sans moduler la hauteur au grain de la mesure. Le passage de
/// l'un à l'autre garde la correction intégrale telle quelle.
#[derive(Debug, Clone)]
struct Regulateur {
    integrale_ppm: f64,
    ecart_lisse: Option<f64>,
    correction: f64,
    temps: f64,
}

/// Pulsation de la boucle pendant l'accrochage, en rad/s.
const PULSATION_ACCROCHAGE: f64 = 0.5;
/// Durée de l'accrochage, en secondes après le départ de B.
const DUREE_ACCROCHAGE_S: f64 = 15.0;
/// Lissage de la mesure pendant l'accrochage, en secondes.
const LISSAGE_ACCROCHAGE_S: f64 = 0.3;

impl Regulateur {
    fn nouveau() -> Self {
        Self {
            integrale_ppm: 0.0,
            ecart_lisse: None,
            correction: 0.0,
            temps: 0.0,
        }
    }

    /// Une mesure d'écart `ecart_s` (remplissage − consigne, en secondes),
    /// prise `dt` secondes après la précédente. Rend la correction relative
    /// du rapport (+1e-4 = +100 ppm d'échantillons produits pour B).
    fn mesurer(&mut self, ecart_s: f64, dt: f64) -> f64 {
        let accrochage = self.temps < DUREE_ACCROCHAGE_S;
        self.temps += dt;
        let (w, lissage) = if accrochage {
            (PULSATION_ACCROCHAGE, LISSAGE_ACCROCHAGE_S)
        } else {
            (PULSATION_PROPRE, LISSAGE_S)
        };
        let lisse = match self.ecart_lisse {
            None => ecart_s,
            Some(prec) => prec + (ecart_s - prec) * (dt / lissage).min(1.0),
        };
        self.ecart_lisse = Some(lisse);
        // L'intégrale est tenue directement en correction (sans unité) :
        // changer de gain ne la fait pas sauter.
        // Un anneau trop plein demande MOINS d'échantillons : signe moins.
        let integrale = self.integrale_ppm - w * w * lisse * dt;
        let brute = integrale - 2.0 * w * lisse;
        // Anti-emballement : quand la sortie bute sur sa borne, l'intégrale
        // ne grandit plus dans le sens de la butée.
        if brute.abs() <= CORRECTION_MAX || (integrale - self.integrale_ppm) * brute < 0.0 {
            self.integrale_ppm = integrale.clamp(-CORRECTION_MAX, CORRECTION_MAX);
        }
        self.correction = brute.clamp(-CORRECTION_MAX, CORRECTION_MAX);
        self.correction
    }
}

/// L'avance visée de B : ce qui est poussé vers B et pas encore entendu.
///
/// La consigne nominale est le retard de A, corrigée de la compensation de
/// latence ; on en retranche la demi-largeur du filtre, retard que B a et que
/// A n'a pas (la trame de B centrée sur l'entrée `x` n'est calculée qu'à
/// l'arrivée de l'entrée `x + COEFFICIENTS / 2`).
///
/// Sans descendre sous une période de B plus 5 ms : juste avant de lire, B
/// doit trouver sa période entière dans l'anneau. Le plancher borne aussi la
/// compensation de latence : avec la consigne de 40 ms et des périodes de
/// 10 ms, B peut être avancé d'environ 25 ms sur A, pas davantage ; l'écart
/// qui reste alors est constant, il ne dérive pas.
#[derive(Debug, Clone, Copy)]
pub struct Consigne {
    trames_b: f64,
    filtre_b: f64,
    canaux: usize,
    cadence_b: f64,
}

impl Consigne {
    /// Avance visée, en trames de B, pour une période de B de `periode_b`
    /// échantillons entrelacés (0 : inconnue) et une compensation de
    /// `compensation_s` secondes (latence de A − latence de B + décalage
    /// manuel : positive, B doit être retardé ; négative, avancé).
    pub fn effective(&self, periode_b: usize, compensation_s: f64) -> f64 {
        let nominale = self.trames_b + compensation_s * self.cadence_b;
        let periode_b = (periode_b / self.canaux) as f64;
        let voulue = nominale - self.filtre_b;
        let plancher = periode_b + 0.005 * self.cadence_b;
        voulue.max(plancher.min(nominale.max(0.0)))
    }

    /// Le seuil, en échantillons entrelacés, à partir duquel le rappel de B
    /// commence à lire : au départ, ce que l'anneau tient juste avant la
    /// première lecture est exactement l'avance de B.
    pub fn seuil_de_depart(&self, periode_b: usize, compensation_s: f64) -> usize {
        // Jamais moins d'une période de B plus 5 ms : une compensation encore
        // provisoire (le retard commun n'est réglé qu'au rappel suivant de A)
        // ferait partir B sur un anneau vide.
        let plancher = (periode_b / self.canaux) as f64 + 0.005 * self.cadence_b;
        let trames = self.effective(periode_b, compensation_s).max(plancher);
        (trames.round() as usize) * self.canaux
    }
}

/// Ce que le recopieur a vécu, pour le journal et le diagnostic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Releve {
    /// Écart lissé du remplissage de B à sa consigne, en millisecondes.
    pub ecart_ms: f64,
    /// Correction courante du rapport, en ppm (avec le bruit de mesure).
    pub correction_ppm: f64,
    /// Dérive d'horloge estimée de B par rapport à A, en ppm : la part
    /// intégrale de la correction, débarrassée du bruit de la mesure.
    pub derive_ppm: f64,
    /// Échantillons que l'anneau de B a refusés (B arrêté ou bloqué).
    pub echantillons_refuses: u64,
    /// Trames recopiées vers B depuis la construction.
    pub trames_produites: u64,
    /// Latences matérielles lissées de A et de B, en millisecondes, lues dans
    /// les horodatages du pilote ; `None` quand il ne les donne pas.
    pub latence_a_ms: Option<f64>,
    pub latence_b_ms: Option<f64>,
    /// Compensation appliquée (latence de A − latence de B + décalage
    /// manuel), en millisecondes.
    pub compensation_ms: f64,
}

/// Constante de temps du lissage des latences lues dans les horodatages.
const LISSAGE_LATENCE_S: f64 = 2.0;

fn lisser(prec: Option<f64>, mesure: Option<f64>, dt: f64) -> Option<f64> {
    match (prec, mesure) {
        (p, None) => p,
        (None, Some(m)) => Some(m),
        (Some(p), Some(m)) => Some(p + (m - p) * (dt / LISSAGE_LATENCE_S).min(1.0)),
    }
}

/// La recopie de A vers B : ligne à retard de A, rééchantillonneur et
/// régulateur de B.
pub struct Recopieur {
    canaux: usize,
    cadence_a: f64,
    cadence_b: f64,
    /// `cadence B / cadence A` : trames de B par trame de A, sans correction.
    rapport_nominal: f64,
    table: TableSinc,
    /// Entrées gardées, entrelacées. `entree[..longueur]` est valide ; la
    /// trame 0 du tampon est la trame absolue `base` (la trame 0 est la
    /// première trame réelle de A ; le silence initial est en négatif).
    entree: Box<[f32]>,
    longueur: usize,
    base: i64,
    /// Position absolue (en trames de A) de la prochaine trame de B.
    position: f64,
    sortie: Box<[f32]>,
    regulateur: Regulateur,
    consigne_trames_b: f64,
    /// Ligne à retard de A pour l'usage à une seule sortie ([`Self::recopier`]).
    /// Avec plusieurs sorties, c'est [`SortieMultiple`] qui porte LA ligne à
    /// retard de A.
    retard: RetardDeA,
    /// Retard commun ajouté à la consigne quand une sortie du groupe est plus
    /// lente que ce que la consigne absorbe ([`SortieMultiple`]).
    retard_supplementaire_s: f64,
    /// Trames de B à ne pas pousser : le recalage d'un réajustement qui
    /// avance B (pendant un silence, donc des zéros qu'on ne joue pas).
    a_sauter: usize,
    echantillons_refuses: u64,
    trames_produites: u64,
    latence_a: Option<f64>,
    latence_b: Option<f64>,
    decalage_manuel_s: f64,
    compensation_s: f64,
}

/// Plus grand retard commun, en secondes : au-delà, le départ et la pause
/// tardent de façon gênante, et une latence de carte de cet ordre est une
/// mesure fausse.
pub const RETARD_MAX_S: f64 = 0.200;

/// La ligne à retard de A : ce que A joue est ce qu'il a reçu `longueur`
/// trames plus tôt. Longueur réglable dans une capacité fixée à la
/// construction (aucune allocation dans le rappel).
pub struct RetardDeA {
    canaux: usize,
    tampon: Box<[f32]>,
    longueur: usize,
    ecriture: usize,
}

impl RetardDeA {
    pub fn nouveau(canaux: usize, longueur_trames: usize, capacite_trames: usize) -> Self {
        let canaux = canaux.max(1);
        let capacite = (capacite_trames.max(longueur_trames) + 1) * canaux;
        Self {
            canaux,
            tampon: vec![0f32; capacite].into_boxed_slice(),
            longueur: longueur_trames * canaux,
            ecriture: 0,
        }
    }

    pub fn longueur_trames(&self) -> usize {
        self.longueur / self.canaux
    }

    /// Changer la longueur. Allonger rejoue du silence tant que le tampon n'a
    /// pas encore été rempli de son : c'est pourquoi [`SortieMultiple`] ne la
    /// règle qu'avant que le premier échantillon réel ne sorte.
    pub fn regler(&mut self, longueur_trames: usize) {
        let max = self.tampon.len() / self.canaux - 1;
        self.longueur = longueur_trames.min(max) * self.canaux;
    }

    pub fn appliquer(&mut self, periode: &mut [f32]) {
        let cap = self.tampon.len();
        for s in periode.iter_mut() {
            self.tampon[self.ecriture] = *s;
            *s = self.tampon[(self.ecriture + cap - self.longueur) % cap];
            self.ecriture += 1;
            if self.ecriture == cap {
                self.ecriture = 0;
            }
        }
    }
}

/// #6132 — la même écoute sur A et sur PLUSIEURS sorties B, chacune corrigée
/// de sa dérive par son propre [`Recopieur`], et un retard commun qui
/// s'adapte à la plus lente.
///
/// Le retard commun vaut la consigne (40 ms) tant qu'elle suffit. Quand une
/// carte a plus de latence que ce que la consigne absorbe (au-delà d'environ
/// 25 ms de plus que A), le retard commun grandit AUTOMATIQUEMENT jusqu'à ce
/// que cette carte-là puisse être nourrie assez tôt, et toutes les autres
/// sont retardées d'autant pour rester alignées. Il est réglé pendant le
/// silence de départ — les latences sont lues dès les premiers rappels — puis
/// figé quand le premier échantillon réel sort de A : le changer en cours
/// d'écoute s'entendrait.
pub struct SortieMultiple<R> {
    cadence_a: f64,
    canaux: usize,
    consigne_s: f64,
    retard: RetardDeA,
    membres: Vec<(Recopieur, R)>,
    trames_traitees: u64,
    fige: bool,
    retard_supplementaire_s: f64,
    /// Un réajustement est demandé (pause, piste suivante) : il attend que la
    /// ligne à retard ne contienne plus que du silence.
    reajustement_demande: bool,
    /// Trames de silence consécutives reçues de A, période courante comprise.
    silence_trames: u64,
    reajustements: u32,
}

/// Sous ce niveau (−100 dB), un échantillon est du silence : celui d'une
/// pause (zéros exacts) ou d'un blanc entre deux pistes, tramage compris.
const SEUIL_DE_SILENCE: f32 = 1e-5;

impl<R: AnneauSecondaire> SortieMultiple<R> {
    pub fn nouvelle(
        canaux: usize,
        cadence_a: u32,
        consigne_s: f64,
        membres: Vec<(Recopieur, R)>,
    ) -> Self {
        let canaux = canaux.max(1);
        let cadence_a = f64::from(cadence_a.max(1));
        let trames = (consigne_s * cadence_a).round() as usize;
        let capacite = ((consigne_s + RETARD_MAX_S) * cadence_a).ceil() as usize;
        Self {
            cadence_a,
            canaux,
            consigne_s,
            retard: RetardDeA::nouveau(canaux, trames, capacite),
            membres,
            trames_traitees: 0,
            fige: false,
            retard_supplementaire_s: 0.0,
            reajustement_demande: false,
            silence_trames: 0,
            reajustements: 0,
        }
    }

    /// Demander que le retard commun soit recalculé — à la pause, à la piste
    /// suivante. Il ne l'est qu'au premier moment où tout ce que A et les B
    /// ont encore à jouer est du silence : jamais pendant le son. Une
    /// transition sans blanc (gapless) attend donc le prochain silence.
    pub fn demander_un_reajustement(&mut self) {
        self.reajustement_demande = true;
    }

    /// Le retard commun voulu, en plus de la consigne, pour la plus lente
    /// des sorties.
    fn retard_requis(&self) -> f64 {
        self.membres
            .iter()
            .map(|(r, a)| r.retard_requis_s(a))
            .fold(0.0, f64::max)
            .min(RETARD_MAX_S)
    }

    fn poser_le_retard(&mut self, requis: f64) {
        self.retard_supplementaire_s = requis;
        self.retard
            .regler(((self.consigne_s + requis) * self.cadence_a).round() as usize);
        for (recopieur, _) in self.membres.iter_mut() {
            recopieur.regler_le_retard_supplementaire(requis);
        }
    }

    /// Un réajustement demandé attend-il encore son silence ?
    pub fn reajustement_en_attente(&self) -> bool {
        self.reajustement_demande
    }

    /// Combien de fois le retard commun a été réajusté depuis le départ.
    pub fn reajustements(&self) -> u32 {
        self.reajustements
    }

    /// Une période de A : recopiée vers chaque B, puis retardée en place du
    /// retard commun.
    pub fn traiter(&mut self, periode: &mut [f32], latence_a_s: Option<f64>) {
        let trames = (periode.len() / self.canaux) as u64;
        if periode.iter().all(|s| s.abs() < SEUIL_DE_SILENCE) {
            self.silence_trames += trames;
        } else {
            self.silence_trames = 0;
        }
        // Réajustement : seulement si la ligne à retard, à l'ancienne comme à
        // la nouvelle longueur, ne rend que du silence. Fait AVANT la recopie
        // de cette période : le recalage de B se mesure comme la régulation,
        // avant que la période ne soit poussée.
        if self.fige && self.reajustement_demande {
            let requis = self.retard_requis();
            let nouvelle = ((self.consigne_s + requis) * self.cadence_a).round() as u64;
            let a_couvrir = nouvelle.max(self.retard.longueur_trames() as u64);
            if self.silence_trames >= a_couvrir {
                self.poser_le_retard(requis);
                for (recopieur, anneau) in self.membres.iter_mut() {
                    recopieur.recaler(&*anneau);
                }
                self.reajustement_demande = false;
                self.reajustements += 1;
            }
        }
        for (recopieur, anneau) in self.membres.iter_mut() {
            recopieur.recopier_vers_b(periode, &*anneau, latence_a_s);
        }
        if !self.fige {
            let requis = self.retard_requis();
            if (requis - self.retard_supplementaire_s).abs() > 1e-4 {
                self.poser_le_retard(requis);
            }
            if self.trames_traitees + trames >= self.retard.longueur_trames() as u64 {
                self.fige = true;
            }
        }
        self.retard.appliquer(periode);
        self.trames_traitees += trames;
    }

    /// Le retard commun appliqué à A, en trames de A.
    pub fn retard_trames(&self) -> usize {
        self.retard.longueur_trames()
    }

    pub fn retard_commun_ms(&self) -> f64 {
        (self.consigne_s + self.retard_supplementaire_s) * 1000.0
    }

    pub fn membres(&self) -> &[(Recopieur, R)] {
        &self.membres
    }
}

/// Plus grande période de A traitée d'un bloc ; une période plus longue est
/// découpée, sans allocation.
const BLOC_MAX: usize = 8192;

impl Recopieur {
    /// `consigne_s` : remplissage visé de l'anneau de B, donc retard ajouté
    /// aux deux sorties. L'anneau de B doit pouvoir en tenir au moins le
    /// double.
    pub fn nouveau(canaux: usize, cadence_a: u32, cadence_b: u32, consigne_s: f64) -> Self {
        let canaux = canaux.max(1);
        let cadence_a = f64::from(cadence_a.max(1));
        let cadence_b = f64::from(cadence_b.max(1));
        let rapport_nominal = cadence_b / cadence_a;
        let coupure = BANDE_PASSANTE * rapport_nominal.min(1.0);
        let retard_trames = (consigne_s * cadence_a).round().max(0.0) as usize;
        let sortie_max =
            ((BLOC_MAX as f64 * rapport_nominal * (1.0 + CORRECTION_MAX)).ceil() as usize + 2)
                * canaux;
        Self {
            canaux,
            cadence_a,
            cadence_b,
            rapport_nominal,
            table: TableSinc::nouvelle(coupure),
            entree: vec![0f32; (BLOC_MAX + 2 * COEFFICIENTS) * canaux].into_boxed_slice(),
            // Le tampon démarre sur `COEFFICIENTS / 2 - 1` trames de silence :
            // la première trame de B est centrée sur la première trame de A.
            longueur: (COEFFICIENTS / 2 - 1) * canaux,
            base: -((COEFFICIENTS / 2 - 1) as i64),
            position: (COEFFICIENTS / 2 - 1) as f64,
            sortie: vec![0f32; sortie_max].into_boxed_slice(),
            regulateur: Regulateur::nouveau(),
            consigne_trames_b: consigne_s * cadence_b,
            retard: RetardDeA::nouveau(canaux, retard_trames, retard_trames),
            retard_supplementaire_s: 0.0,
            a_sauter: 0,
            echantillons_refuses: 0,
            trames_produites: 0,
            latence_a: None,
            latence_b: None,
            decalage_manuel_s: 0.0,
            compensation_s: 0.0,
        }
    }

    /// Le décalage manuel de B par rapport à A, en secondes (positif : B plus
    /// tard). C'est le réglage de secours `sync_delay_ms` des deux zones
    /// (`B − A`) : il s'ajoute à la compensation mesurée, et la remplace
    /// quand le pilote ne donne pas ses horodatages.
    pub fn regler_le_decalage_manuel(&mut self, decalage_s: f64) {
        self.decalage_manuel_s = decalage_s;
    }

    /// Latence de A − latence de B + décalage manuel, bornée à ±200 ms (au-delà,
    /// ce n'est plus une latence de carte, c'est une mesure folle).
    fn compensation_propre(&self) -> f64 {
        let auto = match (self.latence_a, self.latence_b) {
            (Some(a), Some(b)) => a - b,
            _ => 0.0,
        };
        (auto + self.decalage_manuel_s).clamp(-RETARD_MAX_S, RETARD_MAX_S)
    }

    /// La compensation appliquée : la propre, plus le retard commun ajouté.
    fn compensation(&self) -> f64 {
        self.compensation_propre() + self.retard_supplementaire_s
    }

    /// Le retard commun, en plus de la consigne, qu'il faudrait pour que B
    /// tienne sa période entière dans l'anneau malgré sa latence : 0 quand la
    /// consigne suffit. Calculé sur la période que B annonce dès son premier
    /// rappel.
    pub fn retard_requis_s(&self, anneau: &dyn AnneauSecondaire) -> f64 {
        let periode_b = (anneau.periode_annoncee() / self.canaux) as f64 / self.cadence_b;
        let filtre = (COEFFICIENTS / 2) as f64 * self.rapport_nominal / self.cadence_b;
        let disponible = self.consigne_trames_b / self.cadence_b + self.compensation_propre();
        // 5 ms de marge comme le plancher de la consigne, 2 ms de plus pour
        // que le plancher ne morde pas au millième près.
        (periode_b + 0.005 + 0.002 + filtre - disponible).max(0.0)
    }

    /// Recaler B d'un coup sur sa consigne : pousser des zéros s'il doit être
    /// plus en retard, sauter des trames s'il doit l'être moins. Appelé par
    /// [`SortieMultiple`] à un réajustement du retard commun, qui n'a lieu
    /// que pendant un silence : ce qui est ajouté ou sauté ne s'entend pas.
    pub fn recaler(&mut self, anneau: &dyn AnneauSecondaire) {
        let periode_b = anneau.periode_du_lecteur();
        if periode_b == 0 {
            return;
        }
        let ch = self.canaux;
        self.compensation_s = self.compensation();
        let ecoule = anneau.depuis_la_derniere_lecture_s().unwrap_or(0.0);
        let rempli = (anneau.en_attente() + periode_b) as f64 / ch as f64
            - ecoule * self.cadence_b
            - self.a_sauter as f64;
        let visee = self.consigne().effective(periode_b, self.compensation_s);
        let ecart = (visee - rempli).round() as i64;
        if ecart > 0 {
            let mut reste = ecart as usize;
            while reste > 0 {
                let n = reste.min(self.sortie.len() / ch);
                self.sortie[..n * ch].fill(0.0);
                let pris = anneau.pousser(&self.sortie[..n * ch]) / ch;
                self.trames_produites += pris as u64;
                if pris < n {
                    break;
                }
                reste -= n;
            }
        } else {
            self.a_sauter += ecart.unsigned_abs() as usize;
        }
        // L'écart lissé d'avant le saut ne veut plus rien dire ; la part
        // intégrale (la dérive des quartz) reste.
        self.regulateur.ecart_lisse = None;
    }

    /// Le retard commun ajouté par [`SortieMultiple`].
    pub fn regler_le_retard_supplementaire(&mut self, retard_s: f64) {
        self.retard_supplementaire_s = retard_s.max(0.0);
    }

    /// La consigne de remplissage de B, à copier dans le rappel de B pour son
    /// seuil de départ.
    pub fn consigne(&self) -> Consigne {
        Consigne {
            trames_b: self.consigne_trames_b,
            filtre_b: (COEFFICIENTS / 2) as f64 * self.rapport_nominal,
            canaux: self.canaux,
            cadence_b: self.cadence_b,
        }
    }

    /// Rapport courant, correction comprise.
    pub fn rapport(&self) -> f64 {
        self.rapport_nominal * (1.0 + self.regulateur.correction)
    }

    /// Position absolue, en trames de A (avant retard), de la PROCHAINE
    /// trame produite pour B, et numéro de cette trame. Sert à mesurer
    /// l'écart réel dans les tests.
    pub fn repere(&self) -> (u64, f64) {
        (self.trames_produites, self.base as f64 + self.position)
    }

    pub fn releve(&self) -> Releve {
        Releve {
            latence_a_ms: self.latence_a.map(|s| s * 1000.0),
            latence_b_ms: self.latence_b.map(|s| s * 1000.0),
            compensation_ms: self.compensation_s * 1000.0,
            ecart_ms: self.regulateur.ecart_lisse.unwrap_or(0.0) * 1000.0,
            correction_ppm: self.regulateur.correction * 1e6,
            derive_ppm: self.regulateur.integrale_ppm * 1e6,
            echantillons_refuses: self.echantillons_refuses,
            trames_produites: self.trames_produites,
        }
    }

    /// Une période de A, une seule sortie B : `periode` (entrelacée) est
    /// recopiée vers B, puis remplacée en place par sa version retardée — ce
    /// que A doit réellement jouer.
    ///
    /// `latence_a_s` : la latence matérielle de A lue dans les horodatages de
    /// son rappel (`None` si le pilote ne la donne pas).
    pub fn recopier(
        &mut self,
        periode: &mut [f32],
        anneau: &dyn AnneauSecondaire,
        latence_a_s: Option<f64>,
    ) {
        self.recopier_vers_b(periode, anneau, latence_a_s);
        self.retard.appliquer(periode);
    }

    /// Une période de A recopiée vers B, sans toucher à `periode` : la ligne à
    /// retard de A est l'affaire de l'appelant ([`SortieMultiple`]).
    pub fn recopier_vers_b(
        &mut self,
        periode: &[f32],
        anneau: &dyn AnneauSecondaire,
        latence_a_s: Option<f64>,
    ) {
        let ch = self.canaux;
        let trames = periode.len() / ch;
        if trames == 0 {
            return;
        }
        // 0. Latences matérielles : ce que chaque carte met entre son rappel
        //    et le convertisseur. B doit être nourri d'autant plus tôt que sa
        //    latence dépasse celle de A.
        let dt = trames as f64 / self.cadence_a;
        let mesure = |l: Option<f64>| l.filter(|v| v.is_finite() && *v >= 0.0 && *v < 1.0);
        self.latence_a = lisser(self.latence_a, mesure(latence_a_s), dt);
        self.latence_b = lisser(self.latence_b, mesure(anneau.latence_du_lecteur_s()), dt);
        self.compensation_s = self.compensation();
        anneau.publier_l_avance_de_a(
            self.latence_a.unwrap_or(0.0) + self.decalage_manuel_s + self.retard_supplementaire_s,
        );
        // 1. Mesure de l'écart : l'avance de B (poussé mais pas encore
        //    entendu) AVANT cette période, contre la consigne.
        //    Tant que B n'a pas commencé à lire, l'anneau se remplit jusqu'au
        //    seuil de départ par construction : il n'y a rien à corriger, et
        //    réguler là-dessus emballerait l'intégrale avant le départ.
        let periode_b = anneau.periode_du_lecteur();
        if periode_b > 0 {
            let ecoule = anneau.depuis_la_derniere_lecture_s().unwrap_or(0.0);
            // Les trames qu'un recalage a décidé de sauter sont déjà hors du
            // compte : elles ne seront jamais jouées.
            let rempli = (anneau.en_attente() + periode_b) as f64 / ch as f64
                - ecoule * self.cadence_b
                - self.a_sauter as f64;
            let visee = self.consigne().effective(periode_b, self.compensation_s);
            let ecart_s = (rempli - visee) / self.cadence_b;
            self.regulateur.mesurer(ecart_s, dt);
        }
        let pas = 1.0 / self.rapport();

        // 2. Rééchantillonnage par blocs de BLOC_MAX trames.
        for bloc in periode[..trames * ch].chunks(BLOC_MAX * ch) {
            self.reechantillonner(bloc, pas, anneau);
        }
    }

    fn reechantillonner(&mut self, bloc: &[f32], pas: f64, anneau: &dyn AnneauSecondaire) {
        let ch = self.canaux;
        let demi = COEFFICIENTS / 2;
        self.entree[self.longueur..self.longueur + bloc.len()].copy_from_slice(bloc);
        self.longueur += bloc.len();
        let disponibles = self.longueur / ch;

        let mut n = 0usize;
        // La sortie à `position` lit les entrées `i - (demi-1) ..= i + demi`.
        while (self.position.floor() as usize) + demi < disponibles {
            let i = self.position.floor() as usize;
            let frac = (self.position - i as f64) * PHASES as f64;
            let p = (frac.floor() as usize).min(PHASES - 1);
            let mix = (frac - p as f64) as f32;
            let l0 = &self.table.coefficients[p * COEFFICIENTS..(p + 1) * COEFFICIENTS];
            let l1 = &self.table.coefficients[(p + 1) * COEFFICIENTS..(p + 2) * COEFFICIENTS];
            let debut = i + 1 - demi;
            for c in 0..ch {
                let mut a0 = 0f32;
                let mut a1 = 0f32;
                for k in 0..COEFFICIENTS {
                    let x = self.entree[(debut + k) * ch + c];
                    a0 += l0[k] * x;
                    a1 += l1[k] * x;
                }
                self.sortie[n * ch + c] = a0 + (a1 - a0) * mix;
            }
            n += 1;
            self.position += pas;
        }

        let saut = self.a_sauter.min(n);
        self.a_sauter -= saut;
        if n > saut {
            let voulu = (n - saut) * ch;
            let pris = anneau.pousser(&self.sortie[saut * ch..n * ch]);
            self.echantillons_refuses += (voulu - pris) as u64;
            self.trames_produites += (n - saut) as u64;
        }

        // Ne garder que ce que les sorties suivantes liront encore.
        let premiere_utile = (self.position.floor() as usize + 1).saturating_sub(demi);
        if premiere_utile > 0 {
            let debut = premiere_utile * ch;
            self.entree.copy_within(debut..self.longueur, 0);
            self.longueur -= debut;
            self.base += premiere_utile as i64;
            self.position -= premiere_utile as f64;
        }
    }

    pub fn canaux(&self) -> usize {
        self.canaux
    }

    pub fn cadences(&self) -> (f64, f64) {
        (self.cadence_a, self.cadence_b)
    }
}

/// Ce que l'orchestrateur demande à la sortie locale de A : la seconde
/// sortie à ouvrir, et ce qui règle B.
#[derive(Debug, Clone)]
pub struct SecondeSortieDemandee {
    /// Le nom du périphérique B (sans `local:`).
    pub nom: String,
    /// Le curseur de volume de la zone B (millièmes), celui que `set_volume`
    /// et la sourdine de la zone B écrivent. `None` : B suit le volume de A.
    pub volume_utilisateur: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
    /// Le décalage manuel de B sur A, en secondes : `sync_delay_ms` de la
    /// zone B moins celui de la zone A.
    pub decalage_manuel_s: f64,
}

impl SecondeSortieDemandee {
    pub fn nommee(nom: impl Into<String>) -> Self {
        Self {
            nom: nom.into(),
            volume_utilisateur: None,
            decalage_manuel_s: 0.0,
        }
    }
}

/// Le décalage manuel de B sur A, en secondes, depuis les `sync_delay_ms`
/// des deux zones (chacun : de combien retarder SA zone).
pub fn decalage_manuel_s(sync_delay_a_ms: i64, sync_delay_b_ms: i64) -> f64 {
    (sync_delay_b_ms - sync_delay_a_ms) as f64 / 1000.0
}

/// La clé de réglage des sorties suiveuses d'une zone. Valeur : un tableau
/// JSON d'identifiants de sorties locales (`local:<nom>`, comme
/// `output_device_id`) ; absente ou vide = pas de sortie double.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_sortie_double")
}

/// Les périphériques B réglés sur la zone, préfixe `local:` retiré.
pub fn secondes_sorties_de_zone(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
) -> Vec<String> {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .map(|v| lire_la_valeur(&v))
        .unwrap_or_default()
}

/// La valeur du réglage : un tableau JSON de sorties (plusieurs suiveurs), ou
/// une seule sortie en texte (la forme de l'étape 2a).
pub fn lire_la_valeur(valeur: &str) -> Vec<String> {
    let mut noms: Vec<String> = match serde_json::from_str::<Vec<String>>(valeur.trim()) {
        Ok(liste) => liste
            .iter()
            .filter_map(|v| nom_de_peripherique(v))
            .collect(),
        Err(_) => nom_de_peripherique(valeur).into_iter().collect(),
    };
    noms.dedup();
    noms
}

/// La valeur écrite par la route de groupement : un tableau JSON.
pub fn ecrire_la_valeur(sorties: &[String]) -> String {
    serde_json::to_string(sorties).unwrap_or_else(|_| "[]".into())
}

/// `local:<nom>` ou `<nom>` vers `<nom>` ; vide vers `None`.
pub fn nom_de_peripherique(valeur: &str) -> Option<String> {
    let v = valeur.trim();
    let v = v.strip_prefix("local:").unwrap_or(v).trim();
    (!v.is_empty()).then(|| v.to_string())
}

#[cfg(test)]
#[path = "sortie_double_tests.rs"]
mod tests;
