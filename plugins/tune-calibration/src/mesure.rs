//! Le déroulé d'une mesure (C1) : préparation, montée de niveau prudente par
//! salves, balayage complet, analyse, restauration de la zone.
//!
//! ```text
//! POST /zones/{z}/mesures ─► Controleur::lancer
//!    │ volume connu ? entrée connue ? étalonnage lisible ?   sinon : refus
//!    │ capture vers tampon (entree-audio)        ─┐
//!    │ volume de zone forcé à `volume_mesure`     │ fil « calibration-mesure »
//!    │ fournisseur armé, zone lancée              │
//!    │ salve −30 dBFS → capture → LevelPolicy ─┐  │
//!    │   StepUp : +6 dB, salve suivante        │  │
//!    │   Refuse : état Refusée, motif          │  │
//!    │   Accept : balayage complet ◄───────────┘  │
//!    │ analyse_capture → RI, réponse, RT60        │
//!    │ zone arrêtée, volume restauré, capture close
//!    └─► état Terminée (résultat) / Refusée / Annulée / Erreur
//! ```
//!
//! Tout ce qui touche la zone passe par [`HoteZone`] ; tout ce qui touche
//! le micro par `tune_entree_audio::peripheriques::Peripheriques`. Les
//! essais remplacent les deux par des simulations et une pièce synthétique.
//!
//! Garde-fous (note §2.1 et §5) : volume de zone inconnu ⇒ refus ; jamais de
//! salve pleine échelle (départ −30 dBFS) ; plafond joué −6 dBFS tenu par le
//! générateur ; plafond capté −6 dBFS et 90 dB SPL par la politique ;
//! écrêtage ⇒ refus ; restauration du volume dans TOUS les cas de sortie.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tune_calibration::analysis::BandRt60;
use tune_calibration::deconv;
use tune_calibration::fft;
use tune_calibration::level::{LevelDecision, LevelPolicy, ProbeResult, peak_dbfs};
use tune_calibration::mic_cal::MicCalibration;
use tune_calibration::{MeasurementSpec, analyse_capture};
use tune_entree_audio::peripheriques::Peripheriques;
use tune_entree_audio::tampon::CaptureVersTampon;

use crate::fournisseur::FournisseurBalayage;
use crate::generateur::{Consigne, Deroule, Emission, Plan, Voies};
use crate::hote::{ElementBalayage, HoteZone, TraitementsDeZone};

/// Cadence imposée pendant la mesure (note §5 risque 2) : le balayage est
/// servi à 48 kHz, la sortie suit ; la capture doit être à la même cadence.
pub const FREQUENCE_MESURE: u32 = 48_000;

/// Ce que la route reçoit.
#[derive(Debug, Clone)]
pub struct Demande {
    pub zone_id: i64,
    /// Le périphérique d'entrée (nom ou id, comme `/entrees` d'entree-audio).
    pub entree: String,
    /// Voie du micro dans la capture (0 par défaut).
    pub voie_micro: usize,
    /// Nom d'un fichier d'étalonnage déposé, ou `None`.
    pub etalonnage: Option<String>,
    /// Durée du balayage complet (s), bornée [1, 20].
    pub duree_s: f64,
    /// Volume de zone forcé pendant la mesure (0–100).
    pub volume_mesure_pct: f64,
    pub voies: Voies,
    /// Politique de niveau ; les plafonds ne peuvent être que resserrés.
    pub politique: LevelPolicy,
}

impl Demande {
    pub fn defaut(zone_id: i64, entree: &str) -> Self {
        Self {
            zone_id,
            entree: entree.to_string(),
            voie_micro: 0,
            etalonnage: None,
            duree_s: 8.0,
            volume_mesure_pct: 40.0,
            voies: Voies::LesDeux,
            politique: LevelPolicy::default(),
        }
    }

    /// Les bornes que l'utilisateur ne franchit pas, quoi qu'il envoie.
    pub fn borner(mut self) -> Self {
        self.duree_s = self.duree_s.clamp(1.0, 20.0);
        self.volume_mesure_pct = self.volume_mesure_pct.clamp(1.0, 70.0);
        let d = LevelPolicy::default();
        self.politique.max_play_dbfs = self.politique.max_play_dbfs.min(d.max_play_dbfs);
        self.politique.max_capture_dbfs = self.politique.max_capture_dbfs.min(d.max_capture_dbfs);
        self.politique.max_spl_db = self.politique.max_spl_db.min(d.max_spl_db);
        self.politique.start_dbfs = self.politique.start_dbfs.min(d.start_dbfs);
        self.politique.step_db = self.politique.step_db.clamp(1.0, d.step_db);
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "etat", rename_all = "snake_case")]
pub enum Etat {
    Preparation,
    Salve { numero: usize, niveau_dbfs: f64 },
    Balayage { niveau_dbfs: f64 },
    Analyse,
    Terminee,
    Refusee { motif: String },
    Annulee,
    Erreur { message: String },
}

impl Etat {
    pub fn finale(&self) -> bool {
        matches!(
            self,
            Etat::Terminee | Etat::Refusee { .. } | Etat::Annulee | Etat::Erreur { .. }
        )
    }
}

/// Une salve d'essai et ce qu'elle a donné.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Salve {
    pub niveau_joue_dbfs: f64,
    pub crete_captee_dbfs: f64,
    pub rsb_db: f64,
    pub spl_db: Option<f64>,
    pub decision: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rt60Bande {
    pub centre_hz: f64,
    pub edt_s: Option<f64>,
    pub t20_s: Option<f64>,
    pub t30_s: Option<f64>,
    pub dynamique_db: f64,
}

fn fini(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

impl From<&BandRt60> for Rt60Bande {
    fn from(b: &BandRt60) -> Self {
        Self {
            centre_hz: b.center_hz,
            edt_s: fini(b.edt_s),
            t20_s: fini(b.t20_s),
            t30_s: fini(b.t30_s),
            dynamique_db: b.dynamic_range_db,
        }
    }
}

/// Le résultat d'une mesure terminée : ce que l'écran C1 affiche.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Resultat {
    pub frequence: u32,
    pub niveau_joue_dbfs: f64,
    pub salves: Vec<Salve>,
    /// Réponse impulsionnelle : retard, crête, rapport crête/bruit.
    pub ri: ResumeRi,
    /// Réponse en fréquence lissée (1/12 octave) sur grille log.
    pub reponse: Reponse,
    pub rt60: Vec<Rt60Bande>,
    pub etalonnage: Option<String>,
    pub traitements_zone: TraitementsDeZone,
    pub avertissements: Vec<String>,
    pub duree_analyse_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResumeRi {
    pub retard_ms: f64,
    pub crete_dbfs: f64,
    pub rsb_db: f64,
    pub trames: usize,
    pub crete_a_la_trame: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reponse {
    pub lissage: String,
    pub freqs_hz: Vec<f64>,
    pub db: Vec<f64>,
}

/// Ce que `GET /mesures/{id}` rend.
#[derive(Debug, Clone, Serialize)]
pub struct Avancement {
    pub id: String,
    pub zone_id: i64,
    pub entree: String,
    #[serde(flatten)]
    pub etat: Etat,
    pub salves: Vec<Salve>,
    pub depuis_s: f64,
    /// Trames jouées sur la longueur maximale promise : la barre de l'écran.
    pub progression: f64,
    pub avertissements: Vec<String>,
}

struct Interne {
    etat: Etat,
    salves: Vec<Salve>,
    avertissements: Vec<String>,
    resultat: Option<Arc<Resultat>>,
    /// La RI brute (f32) pour `ri.wav`.
    ri_wav: Option<Arc<Vec<u8>>>,
}

pub struct Mesure {
    pub id: String,
    pub demande: Demande,
    pub deroule: Arc<Deroule>,
    debut: Instant,
    interne: Mutex<Interne>,
}

impl Mesure {
    fn verrou(&self) -> std::sync::MutexGuard<'_, Interne> {
        self.interne.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn poser(&self, etat: Etat) {
        let mut i = self.verrou();
        if !i.etat.finale() {
            tracing::info!(mesure = %self.id, etat = ?etat, "calibration_mesure_etat");
            i.etat = etat;
        }
    }

    fn avertir(&self, texte: impl Into<String>) {
        self.verrou().avertissements.push(texte.into());
    }

    pub fn etat(&self) -> Etat {
        self.verrou().etat.clone()
    }

    pub fn resultat(&self) -> Option<Arc<Resultat>> {
        self.verrou().resultat.clone()
    }

    pub fn ri_wav(&self) -> Option<Arc<Vec<u8>>> {
        self.verrou().ri_wav.clone()
    }

    pub fn avancement(&self) -> Avancement {
        let i = self.verrou();
        let max = self.deroule.plan.trames_max().max(1) as f64;
        Avancement {
            id: self.id.clone(),
            zone_id: self.demande.zone_id,
            entree: self.demande.entree.clone(),
            etat: i.etat.clone(),
            salves: i.salves.clone(),
            depuis_s: self.debut.elapsed().as_secs_f64(),
            progression: (self.deroule.trames_rendues() as f64 / max).min(1.0),
            avertissements: i.avertissements.clone(),
        }
    }

    pub fn annuler(&self) {
        self.deroule.annuler();
    }
}

/// Le contrôleur : les mesures en cours et passées, les étalonnages déposés.
pub struct Controleur {
    pub peripheriques: Arc<dyn Peripheriques>,
    pub hote: Arc<dyn HoteZone>,
    pub fournisseur: Arc<FournisseurBalayage>,
    /// Dossier des étalonnages de micro (`<data_dir>/micros/`).
    pub dossier: PathBuf,
    mesures: Mutex<HashMap<String, Arc<Mesure>>>,
    compteur: AtomicU64,
    /// Attente maximale de l'ouverture du flux par la zone.
    pub delai_ouverture: Duration,
    /// Vitesse du temps dans les essais : les attentes de capture sont en
    /// trames, pas en secondes ; ce délai borne seulement l'attente.
    pub delai_capture: Duration,
}

impl Controleur {
    pub fn new(
        peripheriques: Arc<dyn Peripheriques>,
        hote: Arc<dyn HoteZone>,
        fournisseur: Arc<FournisseurBalayage>,
        dossier: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            peripheriques,
            hote,
            fournisseur,
            dossier,
            mesures: Mutex::new(HashMap::new()),
            compteur: AtomicU64::new(0),
            delai_ouverture: Duration::from_secs(20),
            delai_capture: Duration::from_secs(60),
        })
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Mesure>>> {
        self.mesures.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn mesure(&self, id: &str) -> Option<Arc<Mesure>> {
        self.verrou().get(id).cloned()
    }

    pub fn mesures(&self) -> Vec<Arc<Mesure>> {
        let mut v: Vec<_> = self.verrou().values().cloned().collect();
        v.sort_by(|a, b| a.debut.cmp(&b.debut));
        v
    }

    /// Une mesure en cours sur cette zone ?
    pub fn en_cours(&self, zone_id: i64) -> Option<Arc<Mesure>> {
        self.verrou()
            .values()
            .find(|m| m.demande.zone_id == zone_id && !m.etat().finale())
            .cloned()
    }

    pub fn annuler(&self, id: &str) -> bool {
        match self.mesure(id) {
            Some(m) if !m.etat().finale() => {
                m.annuler();
                true
            }
            _ => false,
        }
    }

    /// Annule tout (arrêt du greffon).
    pub fn annuler_tout(&self) {
        for m in self.mesures() {
            m.annuler();
        }
    }

    /// Le texte d'un étalonnage déposé.
    pub fn etalonnage(&self, nom: &str) -> Result<MicCalibration, String> {
        let nom = nom_sur(nom)?;
        let chemin = self.dossier.join(format!("{nom}.txt"));
        let texte = std::fs::read_to_string(&chemin)
            .map_err(|e| format!("étalonnage « {nom} » introuvable : {e}"))?;
        MicCalibration::parse(&texte).map_err(|e| format!("étalonnage « {nom} » : {e}"))
    }

    /// Dépose un étalonnage (texte miniDSP / REW `.cal`), après l'avoir lu.
    pub fn deposer_etalonnage(&self, nom: &str, texte: &str) -> Result<MicCalibration, String> {
        let nom = nom_sur(nom)?;
        let cal = MicCalibration::parse(texte).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dossier).map_err(|e| e.to_string())?;
        std::fs::write(self.dossier.join(format!("{nom}.txt")), texte)
            .map_err(|e| e.to_string())?;
        Ok(cal)
    }

    pub fn etalonnages(&self) -> Vec<String> {
        let Ok(lu) = std::fs::read_dir(&self.dossier) else {
            return Vec::new();
        };
        let mut noms: Vec<String> = lu
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.strip_suffix(".txt").map(str::to_string)
            })
            .collect();
        noms.sort();
        noms
    }

    /// Vérifie, prépare, puis lance la mesure sur son fil. Rend son id.
    pub fn lancer(self: &Arc<Self>, demande: Demande) -> Result<String, String> {
        let demande = demande.borner();
        if let Some(m) = self.en_cours(demande.zone_id) {
            return Err(format!(
                "une mesure est déjà en cours sur cette zone ({})",
                m.id
            ));
        }
        // Garde-fou 1 : volume de zone inconnu ⇒ refus (note §5, risque 5).
        let volume_initial = self.hote.volume(demande.zone_id).ok_or_else(|| {
            format!(
                "zone {} inconnue : volume non lisible, mesure refusée",
                demande.zone_id
            )
        })?;
        let traitements = self.hote.traitements(demande.zone_id);
        if traitements.volume_fixe {
            return Err(
                "la zone est en volume fixe : le niveau joué n'est pas maîtrisable, mesure refusée"
                    .into(),
            );
        }
        let (nom_entree, format) = self.peripheriques.format_natif(&demande.entree)?;
        if format.frequence != FREQUENCE_MESURE {
            return Err(format!(
                "l'entrée « {nom_entree} » capte à {} Hz : la mesure exige {FREQUENCE_MESURE} Hz (régler le périphérique)",
                format.frequence
            ));
        }
        if demande.voie_micro >= format.canaux as usize {
            return Err(format!(
                "voie {} demandée, l'entrée n'en a que {}",
                demande.voie_micro, format.canaux
            ));
        }
        let etalonnage = match &demande.etalonnage {
            Some(n) => Some(self.etalonnage(n)?),
            None => None,
        };
        let id = format!(
            "m{}-{}",
            self.compteur.fetch_add(1, Ordering::Relaxed) + 1,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let plan = Plan::defaut(FREQUENCE_MESURE, demande.duree_s, demande.voies);
        let deroule = Deroule::new(plan);
        let mesure = Arc::new(Mesure {
            id: id.clone(),
            demande: demande.clone(),
            deroule: deroule.clone(),
            debut: Instant::now(),
            interne: Mutex::new(Interne {
                etat: Etat::Preparation,
                salves: Vec::new(),
                avertissements: Vec::new(),
                resultat: None,
                ri_wav: None,
            }),
        });
        if traitements.egaliseur {
            mesure.avertir("égaliseur actif sur la zone : la mesure inclut sa correction");
        }
        if traitements.correction_ir {
            mesure.avertir(
                "correction de pièce (IR) active sur la zone : la mesure inclut son effet",
            );
        }
        if traitements.pure {
            mesure.avertir("mode PURE actif : le volume forcé peut être refusé par le verrou PURE");
        }
        self.verrou().insert(id.clone(), mesure.clone());
        let moi = self.clone();
        std::thread::Builder::new()
            .name("calibration-mesure".into())
            .spawn(move || {
                let sortie = moi.derouler(&mesure, volume_initial, etalonnage, traitements);
                moi.conclure(&mesure, sortie);
            })
            .map_err(|e| format!("fil de mesure : {e}"))?;
        Ok(id)
    }

    /// Conclusion : états finaux, zone arrêtée, fournisseur désarmé.
    fn conclure(&self, mesure: &Arc<Mesure>, sortie: Result<(), Sortie>) {
        self.fournisseur.desarmer(&mesure.id);
        match sortie {
            Ok(()) => mesure.poser(Etat::Terminee),
            Err(Sortie::Refus(motif)) => mesure.poser(Etat::Refusee { motif }),
            Err(Sortie::Annulee) => mesure.poser(Etat::Annulee),
            Err(Sortie::Erreur(message)) => mesure.poser(Etat::Erreur { message }),
        }
    }

    fn derouler(
        &self,
        mesure: &Arc<Mesure>,
        volume_initial: f64,
        etalonnage: Option<MicCalibration>,
        traitements: TraitementsDeZone,
    ) -> Result<(), Sortie> {
        let d = &mesure.demande;
        let deroule = mesure.deroule.clone();
        // Capture d'abord : si le micro ne démarre pas, la zone n'a rien joué.
        let duree_max_s = deroule.plan.duree_max_ms() as f64 / 1000.0 + 30.0;
        let capture = CaptureVersTampon::demarrer(&self.peripheriques, &d.entree, duree_max_s)
            .map_err(Sortie::Erreur)?;
        let zone = ZoneTenue::prendre(
            self.hote.clone(),
            d.zone_id,
            volume_initial,
            d.volume_mesure_pct,
        )
        .map_err(|e| {
            capture.arreter();
            e
        })?;
        let r = self.jouer_et_mesurer(mesure, &capture, &zone, etalonnage.as_ref(), &traitements);
        // Restauration dans tous les cas : arrêt de la zone, volume initial,
        // capture close.
        zone.rendre();
        capture.arreter();
        if capture.debordements() > 0 {
            mesure.avertir(format!(
                "{} débordement(s) de capture : des trames ont été perdues",
                capture.debordements()
            ));
        }
        r
    }

    fn jouer_et_mesurer(
        &self,
        mesure: &Arc<Mesure>,
        capture: &CaptureVersTampon,
        zone: &ZoneTenue,
        etalonnage: Option<&MicCalibration>,
        traitements: &TraitementsDeZone,
    ) -> Result<(), Sortie> {
        let d = &mesure.demande;
        let deroule = mesure.deroule.clone();
        let tampon = capture.tampon().clone();
        let voie = d.voie_micro;

        self.fournisseur.armer(&mesure.id, deroule.clone());
        zone.jouer(ElementBalayage {
            mesure_id: mesure.id.clone(),
            format: deroule.plan.format,
            duree_ms: deroule.plan.duree_max_ms(),
        })?;
        if !deroule.attendre_ouverture(self.delai_ouverture) {
            return Err(if deroule.annulee() {
                Sortie::Annulee
            } else {
                Sortie::Erreur("la zone n'a pas ouvert le balayage".into())
            });
        }

        // Montée de niveau prudente.
        let politique = &d.politique;
        let mut niveau = politique.start_dbfs;
        let mut emissions = 0usize;
        let salve = deroule.salve().clone();
        let niveau_retenu = loop {
            if deroule.annulee() {
                return Err(Sortie::Annulee);
            }
            if emissions >= deroule.plan.salves_max {
                return Err(Sortie::Refus(format!(
                    "{} salves sans atteindre le rapport signal/bruit visé",
                    emissions
                )));
            }
            mesure.poser(Etat::Salve {
                numero: emissions + 1,
                niveau_dbfs: niveau,
            });
            let depart = tampon.trames();
            deroule.consigner(Consigne::Salve {
                niveau_dbfs: niveau,
            });
            let em = self.attendre_emission(&deroule, emissions)?;
            emissions += 1;
            let capte = self.capter_segment(&tampon, voie, depart, &em)?;
            let crete = peak_dbfs(&capte);
            // Déconvolution de la salve : le RSB sur la RI, comme pour la
            // mesure finale.
            let ri = deconv::deconvolve(
                &capte,
                &salve,
                &deconv::ExtractSpec {
                    pre_s: 0.05,
                    post_s: 0.5,
                },
            );
            let rsb = ri.snr_db();
            let spl = etalonnage.and_then(|c| c.spl_from_dbfs(crete));
            let sonde = ProbeResult {
                played_dbfs: em.niveau_dbfs,
                captured_peak_dbfs: crete,
                snr_db: rsb,
                spl_db: spl,
            };
            let decision = politique.decide(&sonde);
            mesure.verrou().salves.push(Salve {
                niveau_joue_dbfs: em.niveau_dbfs,
                crete_captee_dbfs: crete,
                rsb_db: rsb,
                spl_db: spl,
                decision: match &decision {
                    LevelDecision::Accept { .. } => "accept".into(),
                    LevelDecision::StepUp { .. } => "monter".into(),
                    LevelDecision::Refuse { reason, .. } => format!("refus : {reason}"),
                },
            });
            match decision {
                LevelDecision::Accept { play_dbfs } => break play_dbfs,
                LevelDecision::StepUp { next_play_dbfs } => niveau = next_play_dbfs,
                LevelDecision::Refuse { reason, .. } => return Err(Sortie::Refus(reason)),
            }
        };

        // Balayage complet, au niveau retenu (le générateur replafonne).
        if deroule.annulee() {
            return Err(Sortie::Annulee);
        }
        mesure.poser(Etat::Balayage {
            niveau_dbfs: niveau_retenu,
        });
        let depart = tampon.trames();
        deroule.consigner(Consigne::Balayage {
            niveau_dbfs: niveau_retenu,
        });
        let em = self.attendre_emission(&deroule, emissions)?;
        let capte = self.capter_segment(&tampon, voie, depart, &em)?;
        deroule.consigner(Consigne::Fin);
        if deroule.annulee() {
            return Err(Sortie::Annulee);
        }

        mesure.poser(Etat::Analyse);
        let t0 = Instant::now();
        let spec = MeasurementSpec {
            sweep: deroule.plan.balayage.clone(),
            ..MeasurementSpec::room_default(FREQUENCE_MESURE)
        };
        let balayage = deroule.balayage();
        let m = analyse_capture(&capte, balayage, &spec, etalonnage);
        let mut avertissements = mesure.verrou().avertissements.clone();
        if m.ir.snr_db() < 40.0 {
            avertissements.push(format!(
                "rapport crête/bruit de {:.0} dB : mesure bruitée, rapprocher le micro ou réduire le bruit ambiant",
                m.ir.snr_db()
            ));
        }
        let retard_ms = m.ir.delay_ms();
        if retard_ms > 1_500.0 {
            avertissements.push(format!(
                "retard de {retard_ms:.0} ms : sortie réseau probable, synchronie non garantie en C1"
            ));
        }
        let ri_f32: Vec<f32> = m.ir.samples.iter().map(|v| *v as f32).collect();
        let ri_wav = tune_calibration::rew::wav_float32_bytes(FREQUENCE_MESURE, 1, &ri_f32);
        let resultat = Resultat {
            frequence: FREQUENCE_MESURE,
            niveau_joue_dbfs: em.niveau_dbfs,
            salves: mesure.verrou().salves.clone(),
            ri: ResumeRi {
                retard_ms,
                crete_dbfs: fft::to_db(m.ir.peak_amplitude()),
                rsb_db: m.ir.snr_db(),
                trames: m.ir.samples.len(),
                crete_a_la_trame: m.ir.peak_index,
            },
            reponse: Reponse {
                lissage: format!("1/{} octave", spec.smoothing_n),
                freqs_hz: m.response.freqs.clone(),
                db: m.response.magnitude_db.clone(),
            },
            rt60: m.rt60.iter().map(Rt60Bande::from).collect(),
            etalonnage: d.etalonnage.clone(),
            traitements_zone: traitements.clone(),
            avertissements: avertissements.clone(),
            duree_analyse_ms: t0.elapsed().as_millis() as u64,
        };
        let mut i = mesure.verrou();
        i.avertissements = avertissements;
        i.resultat = Some(Arc::new(resultat));
        i.ri_wav = Some(Arc::new(ri_wav));
        Ok(())
    }

    fn attendre_emission(&self, deroule: &Arc<Deroule>, n: usize) -> Result<Emission, Sortie> {
        deroule
            .attendre_emission(n, self.delai_capture)
            .ok_or_else(|| {
                if deroule.annulee() {
                    Sortie::Annulee
                } else {
                    Sortie::Erreur(
                        "le balayage n'est pas parti : la zone a cessé de tirer le flux".into(),
                    )
                }
            })
    }

    /// Les échantillons captés depuis `depart` couvrant le segment `em`
    /// (sa longueur + la garde), une fois qu'ils sont là.
    fn capter_segment(
        &self,
        tampon: &Arc<tune_entree_audio::tampon::Tampon>,
        voie: usize,
        depart: u64,
        em: &Emission,
    ) -> Result<Vec<f64>, Sortie> {
        let garde = (self.garde_trames() as f64).max(0.0) as u64;
        let besoin = depart + em.trames + garde;
        // Attente par tranches, pour voir passer une annulation.
        let echeance = Instant::now() + self.delai_capture;
        loop {
            match tampon.attendre_trames(besoin, Duration::from_millis(200)) {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < echeance => {}
                Ok(None) => {
                    return Err(Sortie::Erreur(format!(
                        "capture trop lente : {} trames sur {besoin} attendues",
                        tampon.trames()
                    )));
                }
                Err(fin) => {
                    return Err(Sortie::Erreur(format!("capture interrompue : {fin:?}")));
                }
            }
        }
        Ok(tampon.extraire(voie, depart, (em.trames + garde) as usize))
    }

    fn garde_trames(&self) -> u64 {
        // La garde du plan (queue de la pièce) plus une seconde : la latence
        // de la sortie décale l'arrivée dans la fenêtre captée, et
        // l'extraction veut 1,5 s APRÈS l'arrivée directe. Capter plus ne
        // coûte que de l'attente : la zone joue du silence après le segment.
        (3.5 * FREQUENCE_MESURE as f64) as u64
    }
}

/// Les sorties d'un déroulé.
#[derive(Debug)]
enum Sortie {
    Refus(String),
    Annulee,
    Erreur(String),
}

/// La zone pendant la mesure : volume forcé, puis rendu — quoi qu'il arrive.
struct ZoneTenue {
    hote: Arc<dyn HoteZone>,
    zone_id: i64,
    volume_initial: f64,
}

impl ZoneTenue {
    fn prendre(
        hote: Arc<dyn HoteZone>,
        zone_id: i64,
        volume_initial: f64,
        volume_mesure: f64,
    ) -> Result<Self, Sortie> {
        hote.regler_volume(zone_id, volume_mesure)
            .map_err(|e| Sortie::Erreur(format!("volume de mesure refusé : {e}")))?;
        Ok(Self {
            hote,
            zone_id,
            volume_initial,
        })
    }

    fn jouer(&self, e: ElementBalayage) -> Result<(), Sortie> {
        self.hote
            .jouer(self.zone_id, e)
            .map_err(|e| Sortie::Erreur(format!("la zone n'a pas démarré le balayage : {e}")))
    }

    fn rendre(self) {
        self.hote.arreter(self.zone_id);
        if let Err(e) = self.hote.regler_volume(self.zone_id, self.volume_initial) {
            tracing::warn!(zone = self.zone_id, error = %e, "calibration_volume_non_restaure");
        }
    }
}

/// Un nom de fichier sûr : lettres, chiffres, point, tiret, souligné.
fn nom_sur(nom: &str) -> Result<String, String> {
    let n = nom.trim();
    if n.is_empty() || n.len() > 80 {
        return Err("nom d'étalonnage vide ou trop long".into());
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        || n.starts_with('.')
    {
        return Err(
            "nom d'étalonnage : lettres, chiffres, point, tiret et souligné seulement".into(),
        );
    }
    Ok(n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_bornes_de_la_demande_ne_se_desserrent_pas() {
        let mut d = Demande::defaut(1, "Micro");
        d.duree_s = 60.0;
        d.volume_mesure_pct = 100.0;
        d.politique.max_play_dbfs = 0.0;
        d.politique.max_capture_dbfs = 0.0;
        d.politique.max_spl_db = 120.0;
        d.politique.start_dbfs = -6.0;
        d.politique.step_db = 20.0;
        let b = d.borner();
        assert_eq!(b.duree_s, 20.0);
        assert_eq!(b.volume_mesure_pct, 70.0);
        assert_eq!(b.politique.max_play_dbfs, -6.0);
        assert_eq!(b.politique.max_capture_dbfs, -6.0);
        assert_eq!(b.politique.max_spl_db, 90.0);
        assert_eq!(b.politique.start_dbfs, -30.0);
        assert_eq!(b.politique.step_db, 6.0);
    }

    #[test]
    fn un_plafond_plus_bas_est_accepte() {
        let mut d = Demande::defaut(1, "Micro");
        d.politique.max_play_dbfs = -12.0;
        d.politique.max_spl_db = 80.0;
        let b = d.borner();
        assert_eq!(b.politique.max_play_dbfs, -12.0);
        assert_eq!(b.politique.max_spl_db, 80.0);
    }

    #[test]
    fn les_noms_d_etalonnage_sont_surs() {
        assert!(nom_sur("UMIK-1_7001234_90deg").is_ok());
        assert!(nom_sur("../etc/passwd").is_err());
        assert!(nom_sur(".cache").is_err());
        assert!(nom_sur("").is_err());
        assert!(nom_sur("a b").is_err());
    }
}
