//! La lecture d'abord : ralentir les traitements de fond pendant qu'une zone
//! joue, et le DIRE (#4681).
//!
//! ## Le cas mesuré
//!
//! #4567 : trois micro-coupures sur Tune Endpoint (OAAT) tombent chacune, à la
//! seconde, sur une ligne `replaygain_album … ecrites=15` — une passe « de pur
//! calcul » qui écrivait en réalité ~60 clés SQLite d'affilée, sur le fil de
//! l'exécuteur asynchrone, PENDANT la lecture. #4572 l'a sortie sur un fil
//! bloquant ; elle continuait pourtant de tourner en pleine écoute, et de
//! prendre le verrou d'écriture unique de la base que le chemin de lecture
//! prend aussi (file, état de zone, historique).
//!
//! ## Ce que ce module ajoute au registre
//!
//! Le registre des traitements ([`super`]) connaît la pause de
//! l'UTILISATEUR. Les passes décodantes (ReplayGain, empreintes, plage
//! dynamique, analyse acoustique) cédaient déjà TOUT à la lecture, chacune de
//! son côté, en relisant `zones.last_play_state` en base. Les autres —
//! l'enrichissement MusicBrainz, les images d'artistes, le scan, le gain
//! d'album — n'avaient aucun frein, et rien ne permettait de savoir, à
//! l'instant d'une coupure, ce qui tournait.
//!
//! Trois choses, et pas une de plus :
//!
//! 1. **Un témoin de lecture en mémoire**, alimenté par le seul point qui
//!    écrit l'état de lecture d'une zone (`ZoneRepo::save_play_state`). Les
//!    boucles le relisent à chaque élément : une lecture atomique, pas une
//!    requête sur la base qui sert la lecture.
//! 2. **Une cadence réduite** : pendant qu'une zone joue, une passe qui
//!    passe par [`ceder_a_la_lecture`] (ou sa jumelle bloquante) marque une
//!    pause de [`PAUSE_EN_LECTURE`] entre deux éléments — deux artistes, deux
//!    pistes à enrichir, deux lots de scan. Elle ne s'ARRÊTE pas : une écoute
//!    de toute une journée ne doit pas geler l'enrichissement jusqu'au soir.
//!    Les passes qui décodent, elles, continuent de tout céder, comme avant.
//! 3. **Un relevé** : quelles passes ont cédé à la lecture, depuis quand une
//!    zone joue — servi par `GET /system/background-tasks` pour qu'une coupure
//!    se rapproche de sa cause, et journalisé en INFO à chaque changement.
//!
//! ## Pourquoi PAS de `nice` sur les écritures en base
//!
//! Le scan abaisse déjà la priorité de ses fils de LECTURE de fichiers
//! (`scanner::walker`, `nice 10` + `ionice` best-effort 7). Faire de même
//! pour les écritures en base serait une inversion de priorité : SQLite n'a
//! qu'UN écrivain, tenu sous un verrou que le chemin de lecture attend aussi.
//! Un fil de fond déprioritisé qui tient ce verrou et se fait préempter fait
//! attendre la lecture PLUS longtemps. Les écritures de fond partent donc sur
//! le pool bloquant ordinaire ([`hors_du_fil_async`]) — hors des fils de
//! l'exécuteur, à priorité normale, et c'est leur CADENCE qu'on réduit.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Pause marquée entre deux éléments d'une passe freinée, tant qu'une zone
/// joue.
///
/// Cinq secondes : l'enrichissement MusicBrainz passe d'une piste par seconde
/// à une toutes les six, un scan de 500 fichiers par lot s'étale — et le
/// travail avance quand même, ce qu'une pause complète ne ferait pas pendant
/// une écoute de huit heures.
pub const PAUSE_EN_LECTURE: Duration = Duration::from_secs(5);

/// Durée pendant laquelle une passe qui a cédé reste « ralentie » au relevé.
///
/// Les passes qui cèdent entièrement dorment 30 s entre deux vérifications
/// (`replaygain::PLAYBACK_BACKOFF_SECS`) : trois fois ce pas, pour qu'une
/// passe freinée ne clignote pas entre deux sondages de l'écran.
pub const FENETRE_RALENTIE: Duration = Duration::from_secs(90);

/// Au-delà de cette durée, une écriture de fond est journalisée : c'est la
/// trace qui aurait manqué à #4567 si la passe d'album avait duré.
const ECRITURE_LONGUE: Duration = Duration::from_millis(50);

/// Les zones qui jouent, par identifiant. Un ENSEMBLE et non un compteur : la
/// même zone annonce « playing » à chaque piste, et un compteur monterait sans
/// jamais redescendre.
static ZONES_QUI_JOUENT: Mutex<BTreeSet<i64>> = Mutex::new(BTreeSet::new());

/// Miroir atomique de « l'ensemble ci-dessus n'est pas vide » — ce que les
/// boucles relisent à chaque élément, sans verrou.
static LECTURE: AtomicBool = AtomicBool::new(false);

/// Depuis quand une zone joue (secondes Unix), 0 au repos.
static DEPUIS: AtomicU64 = AtomicU64::new(0);

/// Les passes qui ont cédé à la lecture, et quand pour la dernière fois.
static CEDEES: Mutex<Option<HashMap<&'static str, Instant>>> = Mutex::new(None);

/// Identifiant de relevé du scan de bibliothèque, qui n'est pas une
/// [`super::Tache`] (voir [`super::pourquoi_le_scan_n_est_pas_suspendable`])
/// mais qui cède à la lecture comme les autres.
pub const ID_SCAN: &str = "scan";

fn maintenant_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Noter l'état de lecture d'une zone. Appelé par `ZoneRepo::save_play_state`,
/// le point unique où cet état s'écrit.
///
/// Seul `"playing"` compte comme lecture : une zone en pause ne consomme ni
/// disque ni réseau, et garder les passes freinées pendant une pause d'une
/// heure n'aurait aucun sens.
///
/// Journalise en INFO les deux transitions — la première zone qui se met à
/// jouer, la dernière qui s'arrête — et seulement elles : la même zone repasse
/// par « playing » à chaque piste, ce qui ne doit rien écrire.
pub fn noter_etat_de_lecture(zone_id: i64, etat: &str) {
    let joue = etat == "playing";
    let mut zones = ZONES_QUI_JOUENT.lock().unwrap_or_else(|e| e.into_inner());
    let avant = !zones.is_empty();
    if joue {
        zones.insert(zone_id);
    } else {
        zones.remove(&zone_id);
    }
    let apres = !zones.is_empty();
    let n = zones.len();
    drop(zones);
    if avant == apres {
        return;
    }
    LECTURE.store(apres, Ordering::Relaxed);
    if apres {
        DEPUIS.store(maintenant_unix(), Ordering::Relaxed);
        tracing::info!(
            zone_id,
            zones = n,
            pause_entre_elements_ms = PAUSE_EN_LECTURE.as_millis() as u64,
            "taches_de_fond_ralenties_pour_la_lecture"
        );
    } else {
        let depuis = DEPUIS.swap(0, Ordering::Relaxed);
        let ralenties = vider_les_cedees();
        tracing::info!(
            zone_id,
            duree_lecture_s = maintenant_unix().saturating_sub(depuis),
            ralenties = ?ralenties,
            "taches_de_fond_reprises_apres_la_lecture"
        );
    }
}

/// Une zone supprimée ne joue plus. Sans cet appel, supprimer une zone en
/// pleine lecture laisserait les passes freinées jusqu'au redémarrage.
pub fn oublier_la_zone(zone_id: i64) {
    noter_etat_de_lecture(zone_id, "stopped");
}

/// Une zone joue-t-elle ? Lecture atomique, gratuite.
pub fn lecture_en_cours() -> bool {
    LECTURE.load(Ordering::Relaxed)
}

/// Noter qu'une passe a cédé à la lecture. Pour les passes qui cèdent par
/// elles-mêmes (le ReplayGain, l'analyse acoustique, la plage dynamique à la
/// demande) : elles restaient muettes au relevé.
///
/// Journalise en INFO la PREMIÈRE fois qu'une passe cède dans la fenêtre —
/// pas à chaque élément, ce qui ferait un torrent.
pub fn noter_cedee(id: &'static str) {
    let mut cedees = CEDEES.lock().unwrap_or_else(|e| e.into_inner());
    let carte = cedees.get_or_insert_with(HashMap::new);
    let maintenant = Instant::now();
    let nouvelle = carte
        .insert(id, maintenant)
        .is_none_or(|avant| maintenant.duration_since(avant) > FENETRE_RALENTIE);
    drop(cedees);
    if nouvelle {
        tracing::info!(tache = id, "tache_de_fond_ralentie_pour_la_lecture");
    }
}

fn vider_les_cedees() -> Vec<&'static str> {
    let mut cedees = CEDEES.lock().unwrap_or_else(|e| e.into_inner());
    let mut ids: Vec<&'static str> = cedees
        .take()
        .map(|c| c.into_keys().collect())
        .unwrap_or_default();
    ids.sort_unstable();
    ids
}

/// Les passes qui ont cédé à la lecture dans la [`FENETRE_RALENTIE`], triées.
pub fn ralenties() -> Vec<&'static str> {
    let cedees = CEDEES.lock().unwrap_or_else(|e| e.into_inner());
    let mut ids: Vec<&'static str> = cedees
        .as_ref()
        .map(|c| {
            c.iter()
                .filter(|(_, quand)| quand.elapsed() <= FENETRE_RALENTIE)
                .map(|(id, _)| *id)
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable();
    ids
}

/// Céder à la lecture, à une frontière propre d'une passe asynchrone.
///
/// Si une zone joue : note la passe au relevé, dort [`PAUSE_EN_LECTURE`] et
/// rend `true`. Sinon rend `false` sans dormir — le cas courant doit être
/// gratuit.
pub async fn ceder_a_la_lecture(id: &'static str) -> bool {
    if !lecture_en_cours() {
        return false;
    }
    noter_cedee(id);
    tokio::time::sleep(PAUSE_EN_LECTURE).await;
    true
}

/// Jumelle BLOQUANTE de [`ceder_a_la_lecture`], pour une passe qui tourne
/// déjà sur un fil bloquant (le scan, entre deux lots).
///
/// ⚠️ Jamais depuis un fil de l'exécuteur : elle y dormirait en le tenant.
pub fn ceder_a_la_lecture_bloquant(id: &'static str) -> bool {
    if !lecture_en_cours() {
        return false;
    }
    noter_cedee(id);
    std::thread::sleep(PAUSE_EN_LECTURE);
    true
}

/// Faire tourner un travail synchrone d'une passe de fond — typiquement ses
/// écritures en base — HORS des fils de l'exécuteur asynchrone.
///
/// Les fils de l'exécuteur servent aussi la lecture (sorties réseau, envoi
/// OAAT, API) : un travail synchrone qui y dure bloque tout ce qui attend son
/// tour sur ce fil. C'est la règle que #4572 a appliquée à la passe d'album ;
/// ce point unique l'étend aux autres passes, et journalise en INFO toute
/// écriture de plus de 50 ms, en disant si une zone jouait.
///
/// Rend `None` si le travail a paniqué — la passe le traite comme un échec
/// d'écriture, sans tomber.
pub async fn hors_du_fil_async<T, F>(id: &'static str, travail: F) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let debut = Instant::now();
    let resultat = tokio::task::spawn_blocking(travail).await;
    let duree = debut.elapsed();
    if duree >= ECRITURE_LONGUE {
        tracing::info!(
            tache = id,
            duree_ms = duree.as_millis() as u64,
            en_lecture = lecture_en_cours(),
            "tache_de_fond_ecriture_longue"
        );
    }
    match resultat {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(tache = id, error = %e, "tache_de_fond_travail_interrompu");
            None
        }
    }
}

/// Le relevé servi par `GET /system/background-tasks`, sous
/// `playback_priority`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleveDePriorite {
    /// Une zone joue : les passes de fond sont freinées.
    pub playback_active: bool,
    /// Les zones qui jouent, par identifiant.
    pub playing_zone_ids: Vec<i64>,
    /// Depuis quand (secondes Unix) ; `None` au repos.
    pub since_epoch_s: Option<u64>,
    /// Les passes qui ont cédé à la lecture dans la fenêtre — identifiants de
    /// [`super::Tache::id`], plus [`ID_SCAN`].
    pub throttled: Vec<&'static str>,
    /// La pause marquée entre deux éléments d'une passe freinée.
    pub pause_between_items_ms: u64,
}

/// Le relevé, sans la moindre requête : l'écran le sonde en boucle.
pub fn releve() -> ReleveDePriorite {
    let zones: Vec<i64> = ZONES_QUI_JOUENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    let depuis = DEPUIS.load(Ordering::Relaxed);
    ReleveDePriorite {
        playback_active: lecture_en_cours(),
        playing_zone_ids: zones,
        since_epoch_s: (depuis != 0).then_some(depuis),
        throttled: ralenties(),
        pause_between_items_ms: PAUSE_EN_LECTURE.as_millis() as u64,
    }
}

/// Remettre le témoin à neuf : aucune zone ne joue, aucune passe n'a cédé.
///
/// `pub` et non `#[cfg(test)]`, pour la même raison que
/// [`super::oublier_pour_les_essais`] : les témoins sont des caisses externes.
pub fn oublier_la_lecture_pour_les_essais() {
    ZONES_QUI_JOUENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    LECTURE.store(false, Ordering::Relaxed);
    DEPUIS.store(0, Ordering::Relaxed);
    vider_les_cedees();
}
