use super::*;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ZonePollerMetrics {
    pub total_polls: u64,
    pub total_errors: u64,
    pub consecutive_errors: u8,
    pub last_latency_ms: u32,
    pub max_latency_ms: u32,
    /// L'appareil annonce toujours jouer alors que la position est arrivee a la
    /// fin de la piste — ou l'a depassee — depuis [`DEPASSEMENT_DUREE_TICKS`]
    /// ticks consecutifs (#2493).
    ///
    /// Champ de CONSTAT, jamais de commande : rien dans le sondeur ne s'appuie
    /// dessus pour avancer ou arreter une piste. Il existe pour qu'un
    /// diagnostic de zone cesse d'affirmer une lecture normale quand Tune sait
    /// deja qu'elle ne l'est pas — soit la lecture est bloquee, soit la duree
    /// connue est fausse, et le sondeur ne peut pas trancher entre les deux.
    pub lecture_au_dela_de_la_duree: bool,
}

/// Plafond du recul sur une zone arrêtée : 2^5 = 32 ticks, soit ~32 s entre
/// deux tentatives quand l'appareil ne répond plus. Assez pour cesser de le
/// noyer, assez court pour repérer une lecture démarrée depuis sa façade.
pub(super) const IDLE_BACKOFF_MAX_SHIFT: u8 = 5;

/// [`IDLE_REPOS_POLL_SECS`] exprimé en ticks de sondeur. Jamais zéro : une
/// cadence de repos plus courte qu'un tick vaut « à chaque tick ».
pub(super) const IDLE_REPOS_POLL_TICKS: u8 = {
    let ticks = (IDLE_REPOS_POLL_SECS * 1000).div_ceil(POLL_INTERVAL_MS);
    if ticks == 0 {
        1
    } else if ticks > u8::MAX as u64 {
        u8::MAX
    } else {
        ticks as u8
    }
};

/// Backoff des sondages sur une zone **arrêtée**.
///
/// Le chemin « zone en lecture » recule déjà après un échec
/// (`ZonePollState::backoff_remaining`), mais celui des zones arrêtées — qui
/// sert à détecter une lecture démarrée hors de Tune — faisait `continue` sans
/// rien mémoriser : un appareil lent ou injoignable était donc re-sondé chaque
/// seconde, indéfiniment. Or `get_status_bounded` abandonne au bout de 5 s
/// pendant que la requête SOAP dessous garde son propre timeout de 10 s et ses
/// deux réessais — les appels s'empilaient sur un renderer qui les traite un par
/// un, jusqu'à ce qu'il ne réponde plus à rien, commande de lecture comprise
/// (Cyrus Stream X2 de JP : 1372 `GetPositionInfo` en échec, contre 3
/// `SetAVTransportURI`).
///
/// `poll_states` ne peut pas porter cet état : il est purgé à chaque tick pour
/// ne garder que les zones en lecture.
#[derive(Debug, Default, Clone)]
pub(super) struct IdlePollBackoff {
    pub(super) consecutive_errors: u8,
    pub(super) remaining: u8,
    /// Comptabilité du journal — distincte du recul, qui lui n'est pas en cause.
    pub(super) journal: JournalSondage,
}

/// Ce que la comptabilité décide de faire d'un échec de plus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceEchecSondage {
    /// Sous le plafond : la ligne complète est émise.
    Detaille,
    /// Au-dessus du plafond, à un palier : une ligne de récapitulatif portant
    /// le TOTAL est émise à la place.
    Recapitulatif,
    /// Au-dessus du plafond, hors palier : rien n'est émis.
    Muet,
}

/// Comptabilité du journal d'un sondage qui échoue, tour après tour (#2566).
///
/// ## Les trois sites, et pourquoi un seul compteur
///
/// | site | fichier | ce qu'un appareil muet coûtait |
/// |---|---|---|
/// | zone au repos | `poller.rs`, branche « repos » | 1 ligne / 33 s — les 79 de Dimitri |
/// | zone **en lecture** | `poller.rs`, branche « lecture » | 1 ligne / 17 s, soit ~212 / h |
/// | HQPlayer | `tune-server/src/background.rs` | 1 ligne / 60 s, **sans aucun recul** |
///
/// Le premier a été borné en v0.9.129 ; le commit qui l'a fait nommait les deux
/// autres comme non traités, et ce sont eux que ce passage-ci rejoint. Le
/// compteur est le même pour les trois — c'est la même panne vue de trois
/// boucles —, mais **l'émission reste locale à chaque site** : `tracing` fige
/// la cible du module au point d'appel, et l'export de diagnostic compte par
/// module (`QUOTA_PAR_MODULE`, #1974). Émettre le bruit d'HQPlayer depuis
/// `tune_core::poller` l'aurait imputé au poller, c'est-à-dire au module qu'on
/// lit précisément quand une lecture ne démarre pas.
///
/// ## Le défaut mesuré
///
/// Dimitri, macOS, v0.9.115, fil 1577 : une zone Chromecast a produit **79
/// lignes `idle_poll_failed_backing_off` identiques**, une par tentative. Le
/// recul exponentiel n'est pas en cause — il plafonnait correctement à
/// `2^IDLE_BACKOFF_MAX_SHIFT` = 32 ticks, et l'extrait le montre
/// (`skip_ticks=32`, 33 s entre deux lignes). C'est le JOURNAL qui n'avait
/// aucun plafond : une ligne par tentative, indéfiniment.
///
/// Au rythme du recul saturé — 32 ticks sautés + 1 tick de tentative, à
/// `POLL_INTERVAL_MS` = 1000 ms — cela fait **une ligne toutes les 33 s et par
/// zone**, soit ~109 lignes par heure. Les 79 échecs de Dimitri couvrent
/// **41 min 16 s**. Un appareil laissé éteint une nuit de 8 h en produit
/// **~870**, et rien ne l'arrête : `consecutive_errors` est un `u8` qui sature
/// à 255 sans jamais cesser de journaliser.
///
/// L'export de diagnostic borne chaque module à un quart de la fenêtre
/// (`QUOTA_PAR_MODULE`, #1974) : 79 lignes prennent déjà un tiers du quota de
/// `tune_core::poller` — le module qu'on lit précisément quand une lecture ne
/// démarre pas.
///
/// ## Le patron, repris de #2890
///
/// Quelques lignes détaillées plafonnées, puis un récapitulatif portant le
/// total — comme `track_insert_failures_truncated` dans `db::track_repo`, et
/// `scan_walk_errors_truncated` dans `scanner::walker`. Une seule différence :
/// là-bas la boucle a une fin (500 pistes), ici elle n'en a pas. Le
/// récapitulatif est donc émis **aux paliers de doublement** — échecs 8, 16,
/// 32, 64, 128… — au lieu d'une fois en sortie de boucle. Une panne coûte
/// ainsi un nombre de lignes **logarithmique** en sa durée, et non linéaire :
/// 79 échecs → 9 lignes, 870 échecs → 12 lignes.
///
/// La fin de panne, elle, est un vrai événement ponctuel : `succes` émet le
/// récapitulatif de clôture avec le total, exactement comme #2890 en sortie de
/// lot.
///
/// ## Ce que cela ne change pas
///
/// Ni la cadence, ni le recul, ni le nombre de tentatives : **aucune décision
/// de sondage ne passe par ici**. Seul le volume du journal change. Un échec
/// isolé reste dit en entier, et un sondage qui réussit sans échec préalable
/// n'émet toujours rien.
#[derive(Debug, Default, Clone)]
pub struct JournalSondage {
    /// Échecs consécutifs, en `u32` : `IdlePollBackoff::consecutive_errors`
    /// est un `u8` qui sature à 255, ce qui rendrait le total faux et les
    /// paliers erratiques au-delà de deux heures de panne.
    pub(super) echecs: u32,
}

impl JournalSondage {
    /// Un échec de plus. Rend ce qu'il faut en dire.
    ///
    /// Publique parce que c'est la **décision** partagée par les trois sites :
    /// chacun l'interroge, puis émet sa propre ligne, avec ses propres champs
    /// et sous sa propre cible de module.
    pub fn compter_echec(&mut self) -> TraceEchecSondage {
        self.echecs = self.echecs.saturating_add(1);
        if self.echecs <= ECHECS_SONDAGE_DETAILLES {
            TraceEchecSondage::Detaille
        } else if self.echecs.is_power_of_two() {
            TraceEchecSondage::Recapitulatif
        } else {
            TraceEchecSondage::Muet
        }
    }

    /// Total d'échecs consécutifs en cours.
    pub fn echecs(&self) -> u32 {
        self.echecs
    }

    /// Un échec de plus, et la trace qui convient est **émise**.
    ///
    /// C'est le point d'émission réel du sondeur : le garde
    /// `tests/journal_sondage_repos.rs` appelle cette fonction-ci, pas une
    /// copie, et compte les lignes que `tracing` reçoit.
    pub fn echec(
        &mut self,
        zone_id: i64,
        device: &str,
        error: &dyn std::fmt::Display,
        skip_ticks: u8,
    ) {
        match self.compter_echec() {
            TraceEchecSondage::Detaille => debug!(
                zone_id,
                device = %device,
                error = %error,
                consecutive_errors = self.echecs,
                skip_ticks,
                "idle_poll_failed_backing_off"
            ),
            TraceEchecSondage::Recapitulatif => debug!(
                zone_id,
                device = %device,
                error = %error,
                echecs = self.echecs,
                detaillees = ECHECS_SONDAGE_DETAILLES,
                skip_ticks,
                "idle_poll_still_failing"
            ),
            TraceEchecSondage::Muet => {}
        }
    }

    /// Le sondage repasse. Émet la clôture de panne s'il y en avait une à
    /// clore, et rien du tout sinon — un sondage qui a toujours réussi ne doit
    /// pas changer d'un iota.
    pub fn succes(&mut self, zone_id: i64, device: &str) {
        if let Some(echecs) = self.cloturer() {
            debug!(
                zone_id,
                device = %device,
                echecs,
                "idle_poll_recovered"
            );
        }
    }

    /// Le sondage repasse : remet le compteur à zéro et rend le total à
    /// annoncer, ou `None` s'il n'y a **rien à clore**.
    ///
    /// Rien à clore, c'est le cas de l'écrasante majorité des tours : un
    /// sondage qui a toujours réussi, et un échec isolé déjà dit en entier. La
    /// clôture n'existe que pour la panne qu'on a cessé de détailler — sans
    /// elle, plafonner masquerait l'ampleur, et un plafond deviendrait une
    /// censure.
    pub fn cloturer(&mut self) -> Option<u32> {
        let echecs = std::mem::take(&mut self.echecs);
        (echecs > ECHECS_SONDAGE_DETAILLES).then_some(echecs)
    }

    /// Un échec de plus sur une zone **en lecture**, et la trace qui convient
    /// est émise.
    ///
    /// Jumelle de [`Self::echec`], et volontairement pas une paramétrisation de
    /// celle-ci : le nom d'un évènement `tracing` est figé au point d'appel,
    /// avec sa cible et son niveau. Le rendre variable ferait de
    /// `poll_failed_backing_off` et `idle_poll_failed_backing_off` un seul et
    /// même point d'appel, indiscernables dans un filtre par cible — pour
    /// n'économiser que l'invocation d'une macro.
    ///
    /// Les champs de la ligne détaillée sont **inchangés** (`zone_id`,
    /// `device`, `error`, `backoff`) : c'est le texte que les journaux déjà
    /// versés portent, et qu'on relit en cherchant une panne.
    pub fn echec_lecture(
        &mut self,
        zone_id: i64,
        device: &str,
        error: &dyn std::fmt::Display,
        backoff: u8,
    ) {
        match self.compter_echec() {
            TraceEchecSondage::Detaille => debug!(
                zone_id,
                device = %device,
                error = %error,
                backoff,
                "poll_failed_backing_off"
            ),
            TraceEchecSondage::Recapitulatif => debug!(
                zone_id,
                device = %device,
                error = %error,
                echecs = self.echecs,
                detaillees = ECHECS_SONDAGE_DETAILLES,
                backoff,
                "poll_still_failing"
            ),
            TraceEchecSondage::Muet => {}
        }
    }

    /// Le sondage d'une zone en lecture repasse. Muet s'il n'avait pas cessé
    /// de parler.
    pub fn succes_lecture(&mut self, zone_id: i64, device: &str) {
        if let Some(echecs) = self.cloturer() {
            debug!(
                zone_id,
                device = %device,
                echecs,
                "poll_recovered"
            );
        }
    }
}

impl IdlePollBackoff {
    /// Faut-il sauter ce tick ? Consomme un tick de recul le cas échéant.
    pub(super) fn should_skip(&mut self) -> bool {
        if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }

    /// Sondage réussi. La suite dépend de ce que l'appareil a répondu.
    ///
    /// Le plein rythme est réservé aux états que la branche « repos » sait
    /// EXPLOITER, pas à ceux qu'on appellerait volontiers « actifs » :
    ///
    /// - `Playing` — reprise d'état, adoption du volume et détection de
    ///   conflit s'y déclenchent, et elles y sont toutes conditionnées ;
    /// - `Transitioning` — transitoire par définition : le freiner
    ///   retarderait l'état qui va suivre, une seconde plus tard.
    ///
    /// `Stopped` et `Paused` ne peuvent rien produire de plus au tick suivant
    /// qu'à celui-ci : la zone retombe à la cadence de repos
    /// [`IDLE_REPOS_POLL_TICKS`] jusqu'à ce que l'appareil bouge (#2263).
    ///
    /// La pause était restée du côté « actif » au motif que la ralentir
    /// ralentirait aussi la reprise d'état et l'adoption du volume. Le motif
    /// ne tenait pas : ces deux-là exigent `status.state == Playing` et ne
    /// font donc RIEN d'un statut en pause. Une zone laissée en pause était
    /// sondée une fois par seconde, sans fin, pour rien.
    pub(super) fn record_success(&mut self, etat: TransportState) {
        self.consecutive_errors = 0;
        self.remaining = match etat {
            TransportState::Stopped | TransportState::Paused => IDLE_REPOS_POLL_TICKS - 1,
            TransportState::Playing | TransportState::Transitioning => 0,
        };
    }

    /// Sondage en échec : recul exponentiel, plafonné.
    pub(super) fn record_failure(&mut self) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        self.remaining = 1u8 << self.consecutive_errors.min(IDLE_BACKOFF_MAX_SHIFT);
    }
}
