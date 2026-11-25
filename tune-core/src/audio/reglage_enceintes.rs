//! TESS — **Réglage des enceintes** (greffon `speaker-setup`, rc14).
//!
//! Le greffon se place en AVANT-DERNIÈRE position de la chaîne : après tous
//! les autres greffons (réaffectation des canaux comprise), juste avant le
//! suréchantillonneur. Il voit donc les voies telles qu'elles partent vers les
//! enceintes, quel que soit leur nombre (1, 2, 6…), et règle, voie par voie :
//!
//! - la **distance** de l'enceinte à la place d'écoute, en cm. Le délai vient
//!   de l'écart avec l'enceinte la plus LOINTAINE : elle n'a aucun délai, les
//!   autres sont retardées de `(d_max − d) / c`, avec c = 343 m/s
//!   ([`VITESSE_DU_SON_M_S`]). Le délai est un nombre ENTIER d'échantillons
//!   (arrondi au plus proche), sans interpolation : un échantillon retardé
//!   ressort identique, au bit près ;
//! - le **gain**, en dB, de [`GAIN_MIN_DB`] à [`GAIN_MAX_DB`] ;
//! - l'**inversion de phase** de la voie, et une inversion globale.
//!
//! # Bit-perfect
//!
//! Réglages neutres (aucun délai, 0 dB, aucune inversion nette) : le
//! processeur ne touche aucun échantillon ([`ProcesseurEnceintes::traiter`]
//! rend `false`), et les hôtes n'entrent même pas dans leur branche de
//! traitement. Un délai seul ne fait que DÉPLACER des échantillons : chaque
//! mot ressort exact. Un facteur de 0 dB vaut exactement 1,0.
//!
//! # Marge (headroom)
//!
//! Dans le moteur, la marge de TESS (`fixed.headroom`, −3 dB en automatique)
//! n'est aujourd'hui que publiée par la résolution : chaque traitement qui peut
//! monter réserve lui-même sa marge (l'égaliseur : `automatic_headroom_db`).
//! Le réglage des enceintes fait de même : un gain positif est couvert par une
//! RÉSERVE interne égale au plus grand gain positif des voies
//! ([`ReglagesEnceintes::reserve_db`]). La voie la plus forte sort donc à
//! 0 dB, les autres en dessous, dans les écarts demandés : aucune voie ne
//! peut écrêter du fait de ce greffon. Le niveau d'ensemble se reprend au
//! volume, comme pour l'égaliseur.
//!
//! # Changement à chaud
//!
//! Un nouveau réglage passe par un FONDU ENCHAÎNÉ de [`RAMPE_MS`] ms entre la
//! sortie de l'ancien réglage et celle du nouveau, calculées sur le même
//! historique : ni saut de délai, ni marche de gain, donc aucun clic. Un
//! réglage reçu pendant un fondu attend la fin de celui-ci.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// L'identifiant du greffon dans le catalogue TESS.
pub const ID_GREFFON: &str = "speaker-setup";
/// La vitesse du son retenue pour le calcul des délais.
pub const VITESSE_DU_SON_M_S: f64 = 343.0;
/// Distance la plus courte acceptée, en cm.
pub const DISTANCE_MIN_CM: f64 = 0.0;
/// Distance la plus longue acceptée, en cm (20 m : au-delà, ce n'est plus une
/// place d'écoute ; 58,3 ms de délai au plus).
pub const DISTANCE_MAX_CM: f64 = 2000.0;
/// Distance d'une voie dont le réglage ne précise pas la distance.
pub const DISTANCE_PAR_DEFAUT_CM: f64 = 300.0;
/// Gain le plus bas accepté, en dB : de quoi aligner une enceinte bien plus
/// sensible que les autres (un caisson, un haut rendement).
pub const GAIN_MIN_DB: f64 = -20.0;
/// Gain le plus haut accepté, en dB. Chaque dB positif est pris sur la marge
/// de TOUTES les voies ([`ReglagesEnceintes::reserve_db`]) : +10 dB couvre
/// les écarts de niveau réels d'une installation (voie centrale, surround),
/// sans faire perdre plus de 10 dB d'ensemble.
pub const GAIN_MAX_DB: f64 = 10.0;
/// Nombre de voies réglables (jusqu'au 9.1.6).
pub const VOIES_MAX: usize = 16;
/// Durée du fondu enchaîné d'un changement de réglage à chaud.
pub const RAMPE_MS: f64 = 20.0;

fn distance_par_defaut() -> f64 {
    DISTANCE_PAR_DEFAUT_CM
}

/// Le réglage d'une voie.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoieEnceinte {
    /// Distance de l'enceinte à la place d'écoute, en cm.
    #[serde(default = "distance_par_defaut")]
    pub distance_cm: f64,
    /// Gain de la voie, en dB.
    #[serde(default)]
    pub gain_db: f64,
    /// Inversion de phase de la voie.
    #[serde(default)]
    pub invert: bool,
}

impl Default for VoieEnceinte {
    fn default() -> Self {
        Self {
            distance_cm: DISTANCE_PAR_DEFAUT_CM,
            gain_db: 0.0,
            invert: false,
        }
    }
}

/// Les réglages du greffon (les `settings` de son maillon).
///
/// Une voie qui arrive sans réglage (le flux a plus de voies que la liste)
/// est neutre : aucun délai, 0 dB, pas d'inversion propre, et elle ne compte
/// pas dans le calcul de l'enceinte la plus lointaine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReglagesEnceintes {
    /// Une entrée par voie, dans l'ordre des voies qui arrivent au greffon.
    #[serde(default)]
    pub channels: Vec<VoieEnceinte>,
    /// Inversion de phase de toutes les voies.
    #[serde(default)]
    pub invert_all: bool,
}

/// Le schéma JSON (draft 2020-12) des réglages, avec ses bornes publiées.
pub fn schema_reglages() -> Value {
    json!({
        "type": "object",
        "properties": {
            "channels": {
                "type": "array",
                "maxItems": VOIES_MAX,
                "items": {
                    "type": "object",
                    "properties": {
                        "distance_cm": {
                            "type": "number",
                            "minimum": DISTANCE_MIN_CM,
                            "maximum": DISTANCE_MAX_CM,
                            "default": DISTANCE_PAR_DEFAUT_CM
                        },
                        "gain_db": {
                            "type": "number",
                            "minimum": GAIN_MIN_DB,
                            "maximum": GAIN_MAX_DB,
                            "default": 0.0
                        },
                        "invert": { "type": "boolean", "default": false }
                    },
                    "additionalProperties": false
                }
            },
            "invert_all": { "type": "boolean", "default": false }
        },
        "additionalProperties": false,
        "x-speed-of-sound-m-s": VITESSE_DU_SON_M_S
    })
}

impl ReglagesEnceintes {
    /// Lit des réglages (objet ou `null` = neutres). `enabled`, que porte
    /// l'ancienne clé de zone, est ignoré. Les bornes sont vérifiées.
    pub fn lire(v: &Value) -> Result<Self, String> {
        let r: Self = match v {
            Value::Null => Self::default(),
            Value::Object(o) => {
                let mut o = o.clone();
                o.remove("enabled");
                serde_json::from_value(Value::Object(o)).map_err(|e| e.to_string())?
            }
            _ => return Err("settings must be a JSON object".into()),
        };
        r.verifier()?;
        Ok(r)
    }

    /// Les bornes : nombres finis, distance et gain dans leurs plages, au
    /// plus [`VOIES_MAX`] voies.
    pub fn verifier(&self) -> Result<(), String> {
        if self.channels.len() > VOIES_MAX {
            return Err(format!(
                "{} channels, at most {VOIES_MAX}",
                self.channels.len()
            ));
        }
        for (i, v) in self.channels.iter().enumerate() {
            if !(v.distance_cm.is_finite()
                && (DISTANCE_MIN_CM..=DISTANCE_MAX_CM).contains(&v.distance_cm))
            {
                return Err(format!(
                    "channel {i}: distance_cm must be within {DISTANCE_MIN_CM}..={DISTANCE_MAX_CM}"
                ));
            }
            if !(v.gain_db.is_finite() && (GAIN_MIN_DB..=GAIN_MAX_DB).contains(&v.gain_db)) {
                return Err(format!(
                    "channel {i}: gain_db must be within {GAIN_MIN_DB}..={GAIN_MAX_DB}"
                ));
            }
        }
        Ok(())
    }

    fn voies(&self, canaux: usize) -> impl Iterator<Item = Option<&VoieEnceinte>> {
        (0..canaux).map(|i| self.channels.get(i))
    }

    /// La distance de l'enceinte la plus lointaine parmi les `canaux` voies
    /// réglées qui arrivent ; `None` si aucune ne l'est.
    fn distance_max(&self, canaux: usize) -> Option<f64> {
        self.voies(canaux)
            .flatten()
            .map(|v| v.distance_cm)
            .fold(None, |m, d| Some(m.map_or(d, |m: f64| m.max(d))))
    }

    /// Le délai de chaque voie, en millisecondes (non arrondi à
    /// l'échantillon : c'est ce que l'écran affiche).
    pub fn retards_ms(&self, canaux: usize) -> Vec<f64> {
        let max = self.distance_max(canaux);
        self.voies(canaux)
            .map(|v| match (v, max) {
                (Some(v), Some(m)) => (m - v.distance_cm) / 100.0 / VITESSE_DU_SON_M_S * 1000.0,
                _ => 0.0,
            })
            .collect()
    }

    /// Le délai de chaque voie en échantillons ENTIERS à `cadence` Hz, arrondi
    /// au plus proche : `round((d_max − d) / c × cadence)`.
    pub fn retards_en_echantillons(&self, canaux: usize, cadence: u32) -> Vec<usize> {
        let max = self.distance_max(canaux);
        self.voies(canaux)
            .map(|v| match (v, max) {
                (Some(v), Some(m)) => {
                    // (cm / 100) / 343 × cadence = cm × cadence / 34 300 :
                    // un seul quotient, exact quand il tombe juste.
                    let ecart_cm = (m - v.distance_cm).max(0.0);
                    (ecart_cm * f64::from(cadence) / (VITESSE_DU_SON_M_S * 100.0)).round()
                        as usize
                }
                _ => 0,
            })
            .collect()
    }

    /// La latence ajoutée : le plus grand délai, en trames à `cadence`.
    pub fn latence_trames(&self, canaux: usize, cadence: u32) -> usize {
        self.retards_en_echantillons(canaux, cadence)
            .into_iter()
            .max()
            .unwrap_or(0)
    }

    /// La réserve (≤ 0 dB) qui couvre le plus grand gain positif des voies.
    pub fn reserve_db(&self, canaux: usize) -> f64 {
        -self
            .voies(canaux)
            .flatten()
            .map(|v| v.gain_db)
            .fold(0.0_f64, f64::max)
    }

    /// Le facteur linéaire de chaque voie : gain, réserve et polarité nette
    /// (voie XOR globale). 0 dB net vaut exactement ±1,0.
    pub fn facteurs(&self, canaux: usize) -> Vec<f64> {
        let reserve = self.reserve_db(canaux);
        self.voies(canaux)
            .map(|v| {
                let gain = v.map_or(0.0, |v| v.gain_db) + reserve;
                let lineaire = if gain == 0.0 {
                    1.0
                } else {
                    10f64.powf(gain / 20.0)
                };
                let inverse = v.is_some_and(|v| v.invert) != self.invert_all;
                if inverse { -lineaire } else { lineaire }
            })
            .collect()
    }

    /// Ces réglages laissent-ils intactes les `canaux` voies qui arrivent ?
    pub fn est_neutre(&self, canaux: usize) -> bool {
        self.facteurs(canaux).iter().all(|f| *f == 1.0)
            && self.distance_max(canaux).is_none_or(|m| {
                self.voies(canaux)
                    .flatten()
                    .all(|v| v.distance_cm == m)
            })
    }

    /// Neutres quel que soit le nombre de voies (1 à [`VOIES_MAX`]) ?
    pub fn est_neutre_partout(&self) -> bool {
        (1..=VOIES_MAX).all(|n| self.est_neutre(n))
    }
}

/// Un échantillon que le processeur sait traiter sans perte (`f32`, `f64`).
pub trait Echantillon: Copy {
    fn en_f64(self) -> f64;
    fn depuis_f64(x: f64) -> Self;
}

impl Echantillon for f32 {
    fn en_f64(self) -> f64 {
        f64::from(self)
    }
    fn depuis_f64(x: f64) -> Self {
        x as f32
    }
}

impl Echantillon for f64 {
    fn en_f64(self) -> f64 {
        self
    }
    fn depuis_f64(x: f64) -> Self {
        x
    }
}

/// Les paramètres effectifs pour un format : délai et facteur par voie.
#[derive(Debug, Clone, PartialEq)]
struct Parametres {
    retards: Vec<usize>,
    facteurs: Vec<f64>,
}

impl Parametres {
    fn neutres(canaux: usize) -> Self {
        Self {
            retards: vec![0; canaux],
            facteurs: vec![1.0; canaux],
        }
    }

    fn de(reglages: &ReglagesEnceintes, canaux: usize, cadence: u32) -> Self {
        Self {
            retards: reglages.retards_en_echantillons(canaux, cadence),
            facteurs: reglages.facteurs(canaux),
        }
    }

    fn neutre(&self) -> bool {
        self.retards.iter().all(|d| *d == 0) && self.facteurs.iter().all(|f| *f == 1.0)
    }
}

/// Le processeur temps réel : une ligne à retard par voie (un anneau
/// commun, entrelacé), un facteur par voie, le fondu des changements.
///
/// - [`Self::traiter`] n'alloue qu'au premier bloc d'un nouveau format
///   (nombre de voies ou cadence) : l'anneau est dimensionné pour le plus
///   grand délai que les bornes permettent à cette cadence ;
/// - [`Self::definir`] est appelé hors temps réel, sous le même verrou.
#[derive(Debug, Clone)]
pub struct ProcesseurEnceintes {
    reglages: ReglagesEnceintes,
    /// Réglage reçu pendant un fondu : appliqué à la fin de celui-ci.
    en_attente: Option<ReglagesEnceintes>,
    canaux: usize,
    cadence: u32,
    actuel: Parametres,
    precedent: Option<Parametres>,
    rampe_totale: usize,
    rampe_restante: usize,
    anneau: Vec<f64>,
    /// Longueur de l'anneau, en trames.
    longueur: usize,
    position: usize,
}

impl ProcesseurEnceintes {
    pub fn new(reglages: ReglagesEnceintes) -> Self {
        Self {
            reglages,
            en_attente: None,
            canaux: 0,
            cadence: 0,
            actuel: Parametres::neutres(0),
            precedent: None,
            rampe_totale: 0,
            rampe_restante: 0,
            anneau: Vec::new(),
            longueur: 0,
            position: 0,
        }
    }

    pub fn reglages(&self) -> &ReglagesEnceintes {
        self.en_attente.as_ref().unwrap_or(&self.reglages)
    }

    /// Le processeur est-il au repos sur des réglages neutres (pour ce
    /// format, ou pour tout format s'il n'en a pas encore vu) ?
    pub fn neutre_au_repos(&self) -> bool {
        self.precedent.is_none()
            && self.en_attente.is_none()
            && if self.canaux == 0 {
                self.reglages.est_neutre_partout()
            } else {
                self.actuel.neutre()
            }
    }

    /// Pose de nouveaux réglages, hors temps réel. Le passage se fait par un
    /// fondu de [`RAMPE_MS`] au bloc suivant ; un fondu en cours se termine
    /// d'abord.
    pub fn definir(&mut self, reglages: ReglagesEnceintes) {
        if self.canaux == 0 {
            self.reglages = reglages;
            return;
        }
        if self.precedent.is_some() {
            self.en_attente = Some(reglages);
            return;
        }
        self.demarrer_le_fondu(reglages);
    }

    fn demarrer_le_fondu(&mut self, reglages: ReglagesEnceintes) {
        let nouveaux = Parametres::de(&reglages, self.canaux, self.cadence);
        self.reglages = reglages;
        if nouveaux == self.actuel {
            return;
        }
        if self.actuel.neutre() {
            // Au repos, l'anneau n'est plus alimenté : son contenu est
            // périmé. Il repart du silence (la voie retardée entre en fondu).
            self.anneau.fill(0.0);
        }
        let ancien = std::mem::replace(&mut self.actuel, nouveaux);
        self.precedent = Some(ancien);
        self.rampe_totale = ((RAMPE_MS / 1000.0) * f64::from(self.cadence)).round().max(1.0) as usize;
        self.rampe_restante = self.rampe_totale;
    }

    /// Vide l'historique (nouvelle piste d'un transcodage par piste entière).
    pub fn reinitialiser(&mut self) {
        self.anneau.fill(0.0);
        self.position = 0;
        if let Some(r) = self.en_attente.take() {
            self.reglages = r;
        }
        self.precedent = None;
        self.rampe_restante = 0;
        if self.canaux > 0 {
            self.actuel = Parametres::de(&self.reglages, self.canaux, self.cadence);
        }
    }

    fn preparer(&mut self, canaux: usize, cadence: u32) {
        if let Some(r) = self.en_attente.take() {
            self.reglages = r;
        }
        self.canaux = canaux;
        self.cadence = cadence;
        self.actuel = Parametres::de(&self.reglages, canaux, cadence);
        self.precedent = None;
        self.rampe_restante = 0;
        let max = (DISTANCE_MAX_CM * f64::from(cadence) / (VITESSE_DU_SON_M_S * 100.0)).ceil()
            as usize;
        self.longueur = max + 1;
        self.anneau.clear();
        self.anneau.resize(self.longueur * canaux, 0.0);
        self.position = 0;
    }

    /// Traite `tampon` (entrelacé, `canaux` voies, `cadence` Hz) en place.
    /// Rend `false` si aucun échantillon n'a été touché (réglages neutres au
    /// repos) : le bloc est alors intact, au bit près.
    pub fn traiter<T: Echantillon>(&mut self, tampon: &mut [T], canaux: u16, cadence: u32) -> bool {
        let canaux = usize::from(canaux);
        if canaux == 0 || cadence == 0 {
            return false;
        }
        if canaux != self.canaux || cadence != self.cadence {
            self.preparer(canaux, cadence);
        }
        if self.precedent.is_none() && self.actuel.neutre() {
            return false;
        }
        let longueur = self.longueur;
        for trame in tampon.chunks_exact_mut(canaux) {
            let base = self.position * canaux;
            let melange = self.precedent.as_ref().map(|p| {
                let fait = self.rampe_totale - self.rampe_restante;
                (p, (fait as f64 + 1.0) / self.rampe_totale as f64)
            });
            for (c, x) in trame.iter_mut().enumerate() {
                self.anneau[base + c] = x.en_f64();
                let lire = |d: usize| {
                    let p = (self.position + longueur - d) % longueur;
                    self.anneau[p * canaux + c]
                };
                let nouveau = self.actuel.facteurs[c] * lire(self.actuel.retards[c]);
                let y = match melange {
                    Some((p, a)) => {
                        let ancien = p.facteurs[c] * lire(p.retards[c]);
                        ancien + a * (nouveau - ancien)
                    }
                    None => nouveau,
                };
                *x = T::depuis_f64(y);
            }
            self.position = (self.position + 1) % longueur;
            if self.precedent.is_some() {
                self.rampe_restante -= 1;
                if self.rampe_restante == 0 {
                    self.precedent = None;
                    if let Some(r) = self.en_attente.take() {
                        self.demarrer_le_fondu(r);
                    }
                }
            }
        }
        true
    }
}

/// « Bypass all » de la chaîne TESS de la zone (`zone_{id}_tess_chain`) :
/// le greffon est coupé, comme les autres traitements. Chaîne absente ou
/// illisible : non.
pub fn bypass_all_de_zone(settings: &crate::db::settings_repo::SettingsRepo, zone_id: i64) -> bool {
    settings
        .get(&crate::tess::cle_chaine_zone(zone_id))
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str::<Value>(&brut).ok())
        .and_then(|v| v.get("bypass_all").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// La clé de zone du greffon : `{"enabled": bool, …réglages}` (écriture
/// croisée de la chaîne TESS, `tess::migration`).
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_speaker_setup")
}

/// Les réglages ARMÉS de la zone : la clé cochée et lisible. Ne regarde ni
/// PURE ni « Bypass all » (voir [`reglages_appliques`]).
pub fn reglages_configures(
    settings: &crate::db::settings_repo::SettingsRepo,
    zone_id: i64,
) -> Option<ReglagesEnceintes> {
    let brut = settings.get(&cle_de_zone(zone_id)).ok().flatten()?;
    let v: Value = serde_json::from_str(&brut).ok()?;
    if v.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    ReglagesEnceintes::lire(&v).ok()
}

/// Les réglages que la lecture APPLIQUE : armés, hors PURE (et mesure de
/// pièce), hors « Bypass all ». `None` : le greffon ne tourne pas.
pub fn reglages_appliques(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
) -> Option<ReglagesEnceintes> {
    if super::audiophile::traitements_contournes(db, zone_id) {
        return None;
    }
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    if bypass_all_de_zone(&settings, zone_id) {
        return None;
    }
    reglages_configures(&settings, zone_id)
}

#[cfg(test)]
#[path = "reglage_enceintes_tests.rs"]
mod tests;
