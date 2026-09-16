//! La plage dynamique à la demande — #4185.
//!
//! « Pas trouvé où lancer l'analyse » (Tades, 0.9.150, fil 1800). Il n'y avait
//! nulle part où la lancer : la mesure n'avait qu'un appelant, le troisième
//! rang de la cascade de fond de [`super::spawn`] — ReplayGain, puis les
//! empreintes, puis la plage dynamique, chacun ne voyant le disque que quand
//! le précédent n'a plus rien. Sur une bibliothèque où la passe nominale a
//! des heures devant elle, le DR n'arrive jamais, et aucun geste ne pouvait
//! le faire passer devant.
//!
//! Ce module est ce geste. Il n'invente pas une seconde mesure : il rappelle
//! [`super::rattraper_un_lot_de_dr`], le même lot, les mêmes gardes, sous le
//! même verrou [`super::ANALYSIS_SLOT`] — jamais deux décodages en même
//! temps (#1576). Ce qu'il change, c'est l'ORDRE : la passe à la demande
//! prend le créneau entre deux lots de la cascade, sans attendre qu'elle ait
//! fini le ReplayGain et les empreintes.
//!
//! ## Un état par processus, tenu par l'appelant
//!
//! Même arbitrage que [`super::progression`] : l'avancement d'une passe est
//! une propriété du processus qui la mène, pas une ligne en base (un serveur
//! tué en plein passage laisserait « en cours » pour toujours). À la
//! différence de `progression`, l'état n'est pas un `static` : la route qui
//! le lit détient déjà l'état du serveur, et une instance se laisse éprouver
//! sans qu'un test en pollue un autre.
//!
//! ## Idempotence
//!
//! Un seul passage à la fois. Un second `demarrer` pendant qu'un passage
//! court rend [`Refus::DejaEnCours`] avec le relevé — la route en fait un 409
//! explicite, jamais un second passage concurrent qui doublerait la charge
//! disque que #1310 et #1576 ont appris à craindre.
//!
//! ## Ce que le passage respecte, et pourquoi il ne le contourne pas
//!
//! * l'analyse coupée (`replaygain_mode = off` ou coche décochée, #2496) :
//!   refus à l'ouverture, arrêt en cours de route. « Désactivé » doit
//!   désactiver, y compris à la demande — le refus NOMME le réglage
//!   ([`super::motif_d_inaction`]) pour que l'utilisateur sache quoi armer ;
//! * la lecture (#1310) : le passage attend, il ne décode pas par-dessus ;
//! * la garde thermique (#1576) : même hystérésis que la cascade.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};

use crate::audio::thermal::ThermalGate;
use crate::db::backend::DbBackend;

use super::{
    ANALYSIS_SLOT, PLAYBACK_BACKOFF_SECS, analysis_enabled, any_zone_playing,
    compter_les_candidats_dr, motif_d_inaction, rattraper_un_lot_de_dr,
};

/// Les délais du passage. Ceux de la cascade en production ; raccourcis par
/// les témoins, qui ne peuvent pas attendre 30 s qu'une zone s'arrête.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// Attente quand une zone joue (#1310).
    pub report_lecture: Duration,
    /// Attente quand la machine est trop chaude (#1576).
    pub report_chaleur: Duration,
    /// Respiration entre deux lots — les pauses par fichier bornent déjà le
    /// travail lui-même.
    pub entre_lots: Duration,
    /// La garde thermique lit un capteur : un banc de test sur une machine
    /// chargée l'éteint, sinon il attendrait qu'elle refroidisse.
    pub garde_thermique: bool,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            report_lecture: Duration::from_secs(PLAYBACK_BACKOFF_SECS),
            report_chaleur: Duration::from_secs(super::THERMAL_RETRY_SECS),
            entre_lots: Duration::from_secs(2),
            garde_thermique: true,
        }
    }
}

/// Après combien de lots rendus VIDES alors que des candidats restent et que
/// rien ne joue, le passage renonce. Un lot vide dans cette situation est une
/// panne de base (`dr_candidate_query_failed` journalisé par le lot) ; boucler
/// dessus toutes les deux secondes ne la réparerait pas, et laisserait la
/// carte « en cours » sur un passage qui n'avance plus.
const LOTS_VIDES_AVANT_ABANDON: u32 = 3;

/// Ce qu'attend le passage quand il n'est pas en train de décoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attente {
    /// Une zone joue : la passe cède (#1310).
    Lecture,
    /// La machine est trop chaude (#1576).
    Chaleur,
    /// Le créneau d'analyse est tenu par une autre passe (ReplayGain,
    /// empreintes, acoustique) : on attend son lot.
    Creneau,
}

impl Attente {
    /// Le code stable que lit le client — même forme que le `waiting_reason`
    /// de `/library/search/acoustic/status` (#4187).
    pub fn code(self) -> &'static str {
        match self {
            Self::Lecture => "playback",
            Self::Chaleur => "thermal",
            Self::Creneau => "analysis_slot",
        }
    }
}

/// Comment le dernier passage s'est terminé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fin {
    /// Plus aucun candidat.
    Terminee,
    /// L'analyse a été coupée en cours de route (#2496).
    AnalyseDesactivee,
    /// Les lots ne rendent plus rien alors que des candidats restent —
    /// voir [`LOTS_VIDES_AVANT_ABANDON`].
    Bloquee,
}

impl Fin {
    /// Le code stable que lit le client.
    pub fn code(self) -> &'static str {
        match self {
            Self::Terminee => "completed",
            Self::AnalyseDesactivee => "analysis_disabled",
            Self::Bloquee => "stalled",
        }
    }
}

/// Le relevé, tel que la route le rend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Releve {
    /// Un passage est ouvert. ⚠️ pas « en train de décoder à cette seconde » :
    /// voir [`Releve::attente`].
    pub actif: bool,
    /// Pistes retirées des candidats depuis l'ouverture — mesurées, reportées
    /// (#1865) ou écartées (#1109). Le NUMÉRATEUR de la jauge.
    pub traitees: i64,
    /// Candidats comptés à l'ouverture, relevé si `traitees` le dépasse (un
    /// scan a pu en ajouter).
    pub total: i64,
    /// Lots rendus par [`super::rattraper_un_lot_de_dr`] depuis l'ouverture.
    pub lots: i64,
    /// Horodatage unix de la dernière mise à jour ; `0` tant que rien n'a
    /// jamais été lancé depuis le démarrage.
    pub maj_epoch: u64,
    /// Ce que le passage attend, `None` quand il décode ou quand il est au
    /// repos.
    pub attente: Option<Attente>,
    /// Comment le DERNIER passage s'est fini ; `None` tant qu'aucun n'a fini.
    pub derniere_fin: Option<Fin>,
}

impl Releve {
    /// Le passage a-t-il déjà dit quoi que ce soit depuis le démarrage ?
    pub fn a_parle(self) -> bool {
        self.maj_epoch > 0
    }
}

/// Pourquoi [`PasseDr::demarrer`] n'a pas ouvert de passage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refus {
    /// Un passage est déjà ouvert — voici où il en est.
    DejaEnCours(Releve),
    /// L'analyse est coupée ; la phrase dit par quel réglage.
    AnalyseDesactivee(&'static str),
}

struct Etat {
    releve: Releve,
    cadence: Cadence,
}

/// La passe de plage dynamique à la demande. Une instance par serveur.
pub struct PasseDr {
    etat: Mutex<Etat>,
}

impl Default for PasseDr {
    fn default() -> Self {
        Self::new()
    }
}

fn maintenant_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PasseDr {
    pub fn new() -> Self {
        Self {
            etat: Mutex::new(Etat {
                releve: Releve {
                    actif: false,
                    traitees: 0,
                    total: 0,
                    lots: 0,
                    maj_epoch: 0,
                    attente: None,
                    derniere_fin: None,
                },
                cadence: Cadence::default(),
            }),
        }
    }

    /// Raccourcit les délais. **Réservé aux témoins.**
    #[doc(hidden)]
    pub fn cadence_pour_les_essais(&self, cadence: Cadence) {
        if let Ok(mut e) = self.etat.lock() {
            e.cadence = cadence;
        }
    }

    /// L'état, tel que la route le rend. Un verrou empoisonné rend le dernier
    /// relevé connu plutôt que de faire tomber une route de diagnostic.
    pub fn releve(&self) -> Releve {
        match self.etat.lock() {
            Ok(e) => e.releve,
            Err(e) => e.into_inner().releve,
        }
    }

    fn modifier(&self, f: impl FnOnce(&mut Releve)) {
        let mut e = match self.etat.lock() {
            Ok(e) => e,
            Err(e) => e.into_inner(),
        };
        f(&mut e.releve);
        e.releve.maj_epoch = maintenant_epoch();
    }

    /// Ouvre un passage et le lance en tâche de fond.
    ///
    /// Dans cet ordre, et l'ordre compte :
    /// 1. un passage déjà ouvert refuse SANS toucher à la base — c'est le cas
    ///    d'un bouton cliqué deux fois, il doit être gratuit ;
    /// 2. l'analyse coupée refuse en nommant le réglage ;
    /// 3. le compte des candidats devient le dénominateur, puis le passage
    ///    est ouvert et lancé.
    ///
    /// `Ok(releve)` est l'état À L'OUVERTURE : `total` dit combien de pistes
    /// le passage va prendre — zéro compris. Un total de zéro est rendu, pas
    /// caché : le passage se ferme alors de lui-même au premier tour, et
    /// l'appelant choisit quoi en dire.
    ///
    /// `sur_avancement` est rappelé à CHAQUE changement d'état (lot rendu,
    /// attente, fin), puis lâché à la fin du passage — c'est par sa
    /// destruction que l'appelant apprend que le passage est fini (le
    /// registre des tâches de fond de `tune-server` tient son garde RAII
    /// dans cette fermeture).
    pub fn demarrer(
        self: &Arc<Self>,
        backend: Arc<dyn DbBackend>,
        sur_avancement: Box<dyn FnMut(&Releve) + Send>,
    ) -> Result<Releve, Refus> {
        {
            let e = match self.etat.lock() {
                Ok(e) => e,
                Err(e) => e.into_inner(),
            };
            if e.releve.actif {
                return Err(Refus::DejaEnCours(e.releve));
            }
        }
        if let Some(motif) = motif_d_inaction(&backend) {
            return Err(Refus::AnalyseDesactivee(motif));
        }
        // Compté HORS verrou : un `COUNT(*)` à cinq `NOT EXISTS` ne se tient
        // pas sous le verrou que la route prend pour lire l'avancement.
        let candidats = compter_les_candidats_dr(&backend).max(0);
        let ouverture = {
            let mut e = match self.etat.lock() {
                Ok(e) => e,
                Err(e) => e.into_inner(),
            };
            // Quelqu'un a pu ouvrir entre les deux verrous : on ne double pas.
            if e.releve.actif {
                return Err(Refus::DejaEnCours(e.releve));
            }
            e.releve = Releve {
                actif: true,
                traitees: 0,
                total: candidats,
                lots: 0,
                maj_epoch: maintenant_epoch(),
                attente: None,
                derniere_fin: None,
            };
            e.releve
        };
        info!(candidats, "dr_passage_demande_ouvert (#4185)");
        let passe = Arc::clone(self);
        tokio::spawn(async move {
            passe.courir(backend, sur_avancement).await;
        });
        Ok(ouverture)
    }

    async fn courir(
        self: Arc<Self>,
        backend: Arc<dyn DbBackend>,
        mut sur_avancement: Box<dyn FnMut(&Releve) + Send>,
    ) {
        let cadence = match self.etat.lock() {
            Ok(e) => e.cadence,
            Err(e) => e.into_inner().cadence,
        };
        let mut thermal = ThermalGate::new();
        let mut lots_vides = 0u32;

        let fin = loop {
            // Mêmes gardes que la cascade, relues À CHAQUE tour : un réglage
            // coupé ou une lecture démarrée pendant un lot ne doit pas
            // attendre le lot suivant pour être vu (#2496, #1310).
            if !analysis_enabled(&backend) {
                break Fin::AnalyseDesactivee;
            }
            if cadence.garde_thermique && thermal.should_hold("dynamic_range") {
                self.attendre(Attente::Chaleur, &mut sur_avancement);
                tokio::time::sleep(cadence.report_chaleur).await;
                continue;
            }
            if any_zone_playing(&backend) {
                self.attendre(Attente::Lecture, &mut sur_avancement);
                tokio::time::sleep(cadence.report_lecture).await;
                continue;
            }
            self.attendre(Attente::Creneau, &mut sur_avancement);
            let n = {
                // Une passe à la fois (#1576) : on prend le créneau entre deux
                // lots de la cascade, on ne s'y ajoute pas.
                let _slot = ANALYSIS_SLOT.lock().await;
                self.modifier(|r| r.attente = None);
                rattraper_un_lot_de_dr(&backend).await
            };
            if n > 0 {
                lots_vides = 0;
                self.modifier(|r| {
                    r.lots += 1;
                    r.traitees += n as i64;
                    if r.traitees > r.total {
                        r.total = r.traitees;
                    }
                });
                sur_avancement(&self.releve());
                tokio::time::sleep(cadence.entre_lots).await;
                continue;
            }
            // Lot vide. Trois lectures possibles, dans l'ordre :
            // plus rien à faire ; le lot a cédé avant son premier fichier
            // (réglage coupé, lecture démarrée) — le tour suivant le verra ;
            // ou une base qui ne répond plus.
            if compter_les_candidats_dr(&backend) <= 0 {
                break Fin::Terminee;
            }
            if !analysis_enabled(&backend) || any_zone_playing(&backend) {
                continue;
            }
            lots_vides += 1;
            if lots_vides >= LOTS_VIDES_AVANT_ABANDON {
                warn!(
                    lots_vides,
                    "dr_passage_demande_bloque — des candidats restent, les lots ne rendent rien"
                );
                break Fin::Bloquee;
            }
            tokio::time::sleep(cadence.entre_lots).await;
        };

        self.modifier(|r| {
            r.actif = false;
            r.attente = None;
            r.derniere_fin = Some(fin);
        });
        let releve = self.releve();
        info!(
            fin = fin.code(),
            traitees = releve.traitees,
            total = releve.total,
            lots = releve.lots,
            "dr_passage_demande_fini (#4185)"
        );
        sur_avancement(&releve);
        // `sur_avancement` tombe ici : c'est le signal de fin pour l'appelant.
    }

    fn attendre(&self, attente: Attente, sur_avancement: &mut Box<dyn FnMut(&Releve) + Send>) {
        let deja = self.releve().attente == Some(attente);
        self.modifier(|r| r.attente = Some(attente));
        // Annoncer chaque ENTRÉE en attente, pas chaque tour d'attente.
        if !deja {
            sur_avancement(&self.releve());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{base_avec_piste, temoins, wav_de_plage_connue};
    use super::*;
    use crate::db::track_metadata_repo::TrackMetadataRepo;

    fn cadence_courte() -> Cadence {
        Cadence {
            report_lecture: Duration::from_millis(20),
            report_chaleur: Duration::from_millis(20),
            entre_lots: Duration::from_millis(5),
            garde_thermique: false,
        }
    }

    fn passe() -> Arc<PasseDr> {
        let p = Arc::new(PasseDr::new());
        p.cadence_pour_les_essais(cadence_courte());
        p
    }

    async fn attendre_la_fin(p: &PasseDr) -> Releve {
        for _ in 0..600 {
            let r = p.releve();
            if !r.actif {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("le passage n'a pas fini en 30 s : {:?}", p.releve());
    }

    /// Le cœur de #4185 : lancé à la main, le passage mesure la piste que la
    /// cascade n'aurait atteinte qu'après tout le reste — et le relevé le dit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn le_passage_a_la_demande_mesure_et_rend_compte() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        TrackMetadataRepo::new(db.clone())
            .set(42, "rg_analyzed", "1700000000")
            .unwrap();

        let p = passe();
        assert!(!p.releve().a_parle(), "rien n'a encore été lancé");
        let ouverture = p
            .demarrer(backend.clone(), Box::new(|_| {}))
            .expect("l'analyse est armée, un candidat existe");
        assert!(ouverture.actif);
        assert_eq!(
            ouverture.total, 1,
            "le dénominateur est le compte des candidats"
        );
        assert_eq!(ouverture.traitees, 0);

        let fin = attendre_la_fin(&p).await;
        let t = temoins(&db);
        assert_eq!(
            t.get("dr_track").map(String::as_str),
            Some("10"),
            "la plage du signal construit vaut 10 dB (cf. wav_de_plage_connue) — \
             le passage à la demande n'a pas mesuré la piste (#4185)"
        );
        assert_eq!(t.get("dr_source").map(String::as_str), Some("analysis"));
        assert_eq!(fin.derniere_fin, Some(Fin::Terminee));
        assert_eq!((fin.traitees, fin.total, fin.lots), (1, 1, 1));
    }

    /// Idempotence : pendant qu'un passage court — ici retenu par une zone en
    /// lecture (#1310) — un second `demarrer` est refusé avec le relevé, et
    /// le passage reprend seul quand la zone s'arrête.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn un_second_demarrage_est_refuse_pendant_le_passage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        TrackMetadataRepo::new(db.clone())
            .set(42, "rg_analyzed", "1700000000")
            .unwrap();
        db.execute(
            "INSERT INTO zones (id, name, last_play_state) VALUES (1, 'Salon', 'playing')",
            &[],
        )
        .unwrap();

        let p = passe();
        let compte = Arc::new(Mutex::new(0u32));
        let compte_cb = Arc::clone(&compte);
        p.demarrer(
            backend.clone(),
            Box::new(move |_| *compte_cb.lock().unwrap() += 1),
        )
        .expect("premier passage ouvert");

        // Le passage attend la lecture : il le dit, et il refuse un doublon.
        for _ in 0..200 {
            if p.releve().attente == Some(Attente::Lecture) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let r = p.releve();
        assert!(r.actif);
        assert_eq!(r.attente, Some(Attente::Lecture), "{r:?}");
        assert_eq!(r.attente.map(Attente::code), Some("playback"));
        match p.demarrer(backend.clone(), Box::new(|_| {})) {
            Err(Refus::DejaEnCours(deja)) => {
                assert!(deja.actif);
                assert_eq!(deja.total, 1);
            }
            autre => panic!("un second passage ne doit pas s'ouvrir : {autre:?}"),
        }
        assert!(
            !temoins(&db).contains_key("dr_track"),
            "rien ne doit être décodé tant qu'une zone joue (#1310)"
        );

        db.execute("UPDATE zones SET last_play_state = 'stopped'", &[])
            .unwrap();
        let fin = attendre_la_fin(&p).await;
        assert_eq!(fin.derniere_fin, Some(Fin::Terminee));
        assert_eq!(fin.traitees, 1);
        assert_eq!(temoins(&db).get("dr_track").map(String::as_str), Some("10"));
        assert!(
            *compte.lock().unwrap() >= 2,
            "l'appelant est prévenu au moins de l'attente et de la fin"
        );

        // Le passage fini, un nouveau s'ouvre — et n'a plus rien à faire.
        let re = p
            .demarrer(backend, Box::new(|_| {}))
            .expect("plus de passage en cours");
        assert_eq!(re.total, 0);
        assert_eq!(attendre_la_fin(&p).await.derniere_fin, Some(Fin::Terminee));
    }

    /// L'analyse coupée (#2496) refuse en NOMMANT le réglage : « Désactivé »
    /// désactive, y compris à la demande, et l'utilisateur sait quoi armer.
    #[tokio::test]
    async fn l_analyse_coupee_refuse_en_nommant_le_reglage() {
        let (db, backend) = base_avec_piste("/nulle/part.flac");
        TrackMetadataRepo::new(db.clone())
            .set(42, "rg_analyzed", "1700000000")
            .unwrap();
        crate::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .set(super::super::MODE_KEY, "off")
            .unwrap();
        let p = passe();
        match p.demarrer(backend, Box::new(|_| {})) {
            Err(Refus::AnalyseDesactivee(motif)) => {
                assert!(motif.contains("replaygain_mode = off"), "{motif}");
            }
            autre => panic!("refus attendu : {autre:?}"),
        }
        assert!(!p.releve().actif);
        assert!(!p.releve().a_parle(), "un refus n'ouvre rien");
    }
}
