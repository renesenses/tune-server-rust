//! Avancement de la passe ReplayGain — #4144.
//!
//! La passe ReplayGain décodait des fichiers pendant des heures sans jamais
//! dire où elle en était : ni `processed`, ni `total`, ni le moindre évènement.
//! L'écran Santé affichait donc `IDLE` — littéralement — pendant toute la
//! durée d'un balayage de 50 000 pistes, et la carte avait RAISON de le dire :
//! aucune route ne lui donnait autre chose.
//!
//! Ce module tient le couple **traitées / total** et l'annonce, à cadence
//! espacée, sur le même bus d'évènements que le scan.
//!
//! ## Pourquoi un état de PROCESSUS, et pas un réglage en base
//!
//! Même arbitrage que [`crate::audio::ecretage`] et que
//! [`crate::scanner::activite`] : l'avancement d'une passe est une propriété du
//! processus qui la mène. L'écrire en base ferait payer une écriture par piste
//! — la passe en traite 25 par lot sur une base qui sert en même temps la
//! lecture — et surtout un serveur tué en plein balayage laisserait une ligne
//! « en cours » que plus rien ne viendrait fermer. Un état de processus naît
//! au repos à chaque démarrage, ce qui est exactement la vérité.
//!
//! ## Pourquoi une cadence, et pas une émission par piste
//!
//! C'est le point que le scan a déjà réglé
//! ([`crate::scanner::walker::CADENCE_PROGRESSION_PARCOURS`]) : sur une
//! bibliothèque de 50 000 titres, émettre à chaque piste produit 50 000
//! messages sur un bus de diffusion dont le tampon fait 256 entrées. Les
//! abonnés lents décrochent, et le WebSocket du client a déjà été vu prendre
//! du retard sous charge (`audio/embedding.rs`, ~380 % CPU). On émet donc à
//! [`CADENCE_PROGRESSION_REPLAYGAIN`], la MÊME que le scan, pour que l'écran
//! reçoive un flux régulier quelle que soit la passe qu'il regarde.
//!
//! Les deux BORDS, eux, sont annoncés sans attendre la cadence : l'ouverture
//! d'une campagne et le retour au repos. Sans cela une passe qui finit juste
//! après une émission laisserait la jauge figée à 98 % jusqu'au prochain
//! sondage du client.

use crate::event_bus::EventBus;
use crate::event_types::EventType;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Cadence des annonces d'avancement de la passe ReplayGain.
///
/// Deux secondes — la même valeur que
/// [`crate::scanner::walker::CADENCE_PROGRESSION_PARCOURS`] et que l'émission
/// par lots de l'import. Ce n'est pas une coïncidence qu'on pourrait faire
/// diverger : l'écran Santé affiche les deux passes l'une sous l'autre, et
/// deux cadences différentes s'y liraient comme deux vitesses différentes.
pub const CADENCE_PROGRESSION_REPLAYGAIN: Duration = Duration::from_secs(2);

/// Ce que la passe ReplayGain sait dire d'elle-même à un instant donné.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Avancement {
    /// Une campagne est ouverte : la passe a du travail devant elle.
    ///
    /// ⚠️ `actif` ne veut pas dire « en train de décoder à cette seconde » : la
    /// passe cède à la lecture (#1310) et à la garde thermique (#1576) sans
    /// fermer sa campagne. Elle a du travail, elle ne l'a pas fini.
    pub actif: bool,
    /// Pistes retirées du balayage depuis l'ouverture de la campagne — qu'elles
    /// aient été mesurées, reportées (#1865) ou écartées pour leur taille
    /// (#1109). C'est le NUMÉRATEUR de la jauge : ce qui ne ressortira pas de
    /// la prochaine requête de candidats.
    pub traitees: i64,
    /// Candidats comptés à l'ouverture de la campagne, plus les pistes déjà
    /// traitées. Voir [`ouvrir_si_besoin`] pour pourquoi il n'est pas recompté
    /// à chaque lot.
    pub total: i64,
    /// Horodatage unix de la dernière mise à jour. `0` quand la passe n'a
    /// jamais rien annoncé depuis le démarrage — état d'un serveur qui vient de
    /// démarrer, à ne pas confondre avec une passe finie.
    pub maj_epoch: u64,
}

impl Avancement {
    /// La passe a-t-elle déjà dit quoi que ce soit depuis le démarrage ?
    ///
    /// Sert à la route : un `traitees = 0, total = 0` jamais renseigné ne doit
    /// pas s'afficher comme « 0 piste sur 0 », qui se lirait comme une
    /// bibliothèque vide.
    pub fn a_parle(self) -> bool {
        self.maj_epoch > 0
    }
}

struct Etat {
    actif: bool,
    traitees: i64,
    total: i64,
    maj_epoch: u64,
    /// `None` tant qu'aucune annonce n'est partie : la première ne doit pas
    /// attendre la cadence.
    derniere_emission: Option<Instant>,
}

static ETAT: Mutex<Etat> = Mutex::new(Etat {
    actif: false,
    traitees: 0,
    total: 0,
    maj_epoch: 0,
    derniere_emission: None,
});

/// Le bus, branché UNE fois au démarrage du serveur.
///
/// `replaygain::spawn` ne reçoit que le dépôt : la passe vit dans `tune-core`
/// et le bus est construit par `tune-server`. Plutôt que de faire traverser un
/// `Arc<EventBus>` à cinq signatures de fonctions dont aucune n'émet, on le
/// dépose ici — même montage que le compteur d'écrêtage, lu par la route de
/// diagnostic sans qu'aucun étage audio ne connaisse l'API.
///
/// Non branché, tout continue de marcher : les compteurs montent, la route les
/// rend, seul le fil d'évènements se tait. C'est ce que voient les tests.
static BUS: OnceLock<Arc<EventBus>> = OnceLock::new();

/// Un seul avertissement si le bus n'est pas branché : la passe appelle
/// `avancer` des dizaines de milliers de fois.
static ABSENCE_DE_BUS_DITE: AtomicBool = AtomicBool::new(false);

/// Branche le bus d'évènements. Appelé une fois, au démarrage du serveur.
pub fn brancher_le_bus(bus: Arc<EventBus>) {
    let _ = BUS.set(bus);
}

fn maintenant_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Ouvre une campagne si aucune n'est en cours, et annonce le départ.
///
/// `compter_les_restants` n'est appelé QUE quand la campagne s'ouvre : c'est un
/// `COUNT(*)` avec trois `NOT EXISTS` sur `track_metadata`, qui se paie sur une
/// grosse bibliothèque. Le rejouer à chaque lot de 25 pistes le ferait tourner
/// deux mille fois sur un balayage de 50 000 titres, pour un total qui ne
/// change pas.
///
/// Le total est donc figé pour la durée de la campagne. Un scan qui ajoute des
/// pistes pendant le balayage peut faire dépasser les traitées au-dessus du
/// total : [`avancer`] relève alors le total au lieu d'afficher 103 %.
pub fn ouvrir_si_besoin(compter_les_restants: impl FnOnce() -> i64) {
    // Lu AVANT de compter, et le verrou relâché : le `COUNT(*)` ne doit pas se
    // tenir sous le verrou que la route prend pour lire l'avancement.
    if ETAT.lock().map(|e| e.actif).unwrap_or(false) {
        return;
    }
    let restants = compter_les_restants().max(0);
    let instantane = {
        let Ok(mut etat) = ETAT.lock() else {
            return;
        };
        // Quelqu'un a pu ouvrir entre-temps — impossible avec la boucle
        // actuelle (une seule passe), mais on ne le jette pas pour autant.
        if etat.actif {
            return;
        }
        etat.actif = true;
        etat.traitees = 0;
        etat.total = restants;
        etat.maj_epoch = maintenant_epoch();
        etat.derniere_emission = Some(Instant::now());
        releve_sous_verrou(&etat)
    };
    emettre(instantane, "started");
}

/// Une piste de moins à traiter.
///
/// Appelée sur CHAQUE sortie de la boucle de balayage qui retire la piste des
/// candidats — mesurée, reportée ou écartée. Pas sur les abandons au profit de
/// la lecture : la piste repassera.
pub fn avancer() {
    let instantane = {
        let Ok(mut etat) = ETAT.lock() else {
            return;
        };
        if !etat.actif {
            // Une avance hors campagne n'a pas de dénominateur ; l'ignorer vaut
            // mieux qu'une jauge sur un total de zéro.
            return;
        }
        etat.traitees += 1;
        // Le scan a pu ajouter des pistes depuis l'ouverture : mieux vaut un
        // total qui monte qu'une jauge au-delà de 100 %.
        if etat.traitees > etat.total {
            etat.total = etat.traitees;
        }
        etat.maj_epoch = maintenant_epoch();
        let du = etat
            .derniere_emission
            .map(|t| t.elapsed() >= CADENCE_PROGRESSION_REPLAYGAIN)
            .unwrap_or(true);
        if !du {
            return;
        }
        etat.derniere_emission = Some(Instant::now());
        releve_sous_verrou(&etat)
    };
    emettre(instantane, "progress");
}

/// La passe n'a plus rien à traiter — ou son réglage vient d'être coupé.
///
/// Ferme la campagne et l'annonce UNE fois. Rappelée au repos, elle ne dit
/// rien : la boucle repasse ici toutes les `IDLE_SLEEP_SECS`, et réémettre à
/// chaque tour noierait le bus d'un évènement qui ne raconte rien de neuf —
/// exactement la faute que la cadence évite du côté du travail.
pub fn au_repos() {
    let instantane = {
        let Ok(mut etat) = ETAT.lock() else {
            return;
        };
        if !etat.actif {
            return;
        }
        etat.actif = false;
        // Le compteur n'est PAS remis à zéro : « 4 812 pistes traitées, plus
        // rien en attente » est ce que l'écran doit montrer après la passe. Un
        // retour à 0/0 se lirait comme « rien n'a jamais tourné ».
        etat.maj_epoch = maintenant_epoch();
        etat.derniere_emission = Some(Instant::now());
        releve_sous_verrou(&etat)
    };
    emettre(instantane, "idle");
}

fn releve_sous_verrou(etat: &Etat) -> Avancement {
    Avancement {
        actif: etat.actif,
        traitees: etat.traitees,
        total: etat.total,
        maj_epoch: etat.maj_epoch,
    }
}

/// L'avancement, tel que la route le rend.
pub fn releve() -> Avancement {
    match ETAT.lock() {
        Ok(etat) => releve_sous_verrou(&etat),
        // Un verrou empoisonné ne doit pas faire tomber une route de
        // diagnostic : on rend un état neutre, que `a_parle()` distingue.
        Err(_) => Avancement {
            actif: false,
            traitees: 0,
            total: 0,
            maj_epoch: 0,
        },
    }
}

/// Remet l'état à neuf. **Réservé aux témoins** : l'état est global au
/// processus, un banc qui en enchaîne deux doit pouvoir repartir de zéro.
#[doc(hidden)]
pub fn reinitialiser_pour_les_essais() {
    if let Ok(mut etat) = ETAT.lock() {
        etat.actif = false;
        etat.traitees = 0;
        etat.total = 0;
        etat.maj_epoch = 0;
        etat.derniere_emission = None;
    }
}

fn emettre(a: Avancement, phase: &'static str) {
    let Some(bus) = BUS.get() else {
        if !ABSENCE_DE_BUS_DITE.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                "replaygain_progression_sans_bus — avancement tenu, non diffusé \
                 (aucun appel à brancher_le_bus)"
            );
        }
        return;
    };
    // Le nom est écrit EN TOUTES LETTRES ici, et nulle part ailleurs : le
    // garde-fou de `event_types.rs` cherche l'énumération en premier argument
    // d'un `emit_typed`, et un relais par paramètre ne compterait pas.
    bus.emit_typed(
        EventType::ReplayGainProgress,
        json!({
            "phase": phase,
            "active": a.actif,
            "processed": a.traitees,
            "total": a.total,
            "remaining": (a.total - a.traitees).max(0),
            "updated_at": a.maj_epoch,
        }),
    );
}
