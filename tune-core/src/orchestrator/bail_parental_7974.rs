//! #7974 — contrôle parental, étape 2 : le bail d'écoute appliqué à la
//! lecture.
//!
//! Les règles (plages, budgets, refus) vivent dans `db::bail_d_ecoute` ; ici,
//! leur application, en quatre points :
//!
//! - **départ** : `play_par_l_entree` (toutes les entrées de lecture :
//!   bibliothèque, services, radios, podcasts, file, alarmes, renderer UPnP,
//!   fin de piste du sondeur) et `play_without_history` (recréations de
//!   flux : seek, relance radio, reconnexion). Le choix de la sortie (locale, UPnP/DLNA,
//!   AirPlay, Chromecast, OpenHome…) vient après : un refus les couvre toutes ;
//! - **reprise** après pause ;
//! - **enchaînement sans blanc** : la piste suivante n'est pas armée sur le
//!   renderer si le bail finit avant elle ;
//! - **décompte** : le sondeur appelle [`PlaybackOrchestrator::decompter_le_bail`]
//!   à chaque tour pour chaque zone. Seule une zone `Playing` compte : une
//!   pause n'avance pas le compteur.
//!
//! Le profil qui écoute est celui de la session de la zone, sinon le profil
//! actif du serveur (même repli que les titres bannis de la file). Un profil
//! sans politique, ou dont la politique ne limite que les listes, ne demande
//! aucun bail : rien ne change.
//!
//! Fin de bail pendant la lecture : si la piste en cours finit dans le délai
//! d'annonce, elle va à son terme et la suivante est refusée. Sinon (radio,
//! piste longue), la fin est annoncée (`parental_lease_ending`, avec le
//! délai) et la zone est arrêtée à son échéance (`parental_lease_ended`) : le
//! son n'est pas coupé sans prévenir.

use super::PlaybackOrchestrator;
use crate::db::bail_d_ecoute::{self, Bail, Refus};
use crate::playback::{PlayState, ZoneState};
use chrono::NaiveDateTime;
use std::time::{Duration, Instant};

/// Délai entre l'annonce de fin de bail et l'arrêt de la zone.
pub const DELAI_D_ANNONCE: Duration = Duration::from_secs(30);

/// Le temps écouté s'écrit en base par paquets de cette durée au moins.
const PAS_D_ECRITURE_MS: i64 = 10_000;

/// Un écart entre deux tours plus long que ceci (veille, sondeur bloqué) ne
/// compte que pour ce plafond.
const ECART_MAX_MS: i64 = 5_000;

/// Le suivi d'une zone en lecture par un profil supervisé.
#[derive(Debug, Clone)]
pub(super) struct SuiviDuBail {
    profile_id: i64,
    /// Politique limitante : on compte et on surveille la fin.
    supervise: bool,
    dernier_tour: Instant,
    ms_non_ecrites: i64,
    /// La fin du bail, si plage ou budget la bornent.
    fin: Option<Instant>,
    /// L'arrêt annoncé, à cette échéance.
    arret_a: Option<Instant>,
}

/// Ce que le tour du sondeur doit faire pour une zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GesteDeFinDeBail {
    Rien,
    /// Annoncer la fin, l'arrêt suivra après [`DELAI_D_ANNONCE`].
    Annoncer,
    /// Laisser finir la piste en cours ; la suivante sera refusée.
    LaisserFinirLaPiste,
    /// Arrêter la zone maintenant.
    Arreter,
}

/// La décision de fin de bail, pure : `reste_piste_ms` est ce qu'il reste de
/// la piste en cours (`None` : flux sans durée).
pub fn geste_de_fin_de_bail(
    maintenant: Instant,
    fin: Option<Instant>,
    arret_a: Option<Instant>,
    reste_piste_ms: Option<i64>,
) -> GesteDeFinDeBail {
    if let Some(a) = arret_a {
        return if maintenant >= a {
            GesteDeFinDeBail::Arreter
        } else {
            GesteDeFinDeBail::Rien
        };
    }
    match fin {
        Some(f) if maintenant >= f => match reste_piste_ms {
            Some(r) if r <= DELAI_D_ANNONCE.as_millis() as i64 => {
                GesteDeFinDeBail::LaisserFinirLaPiste
            }
            _ => GesteDeFinDeBail::Annoncer,
        },
        _ => GesteDeFinDeBail::Rien,
    }
}

impl PlaybackOrchestrator {
    /// Le profil qui écoute sur la zone.
    pub(super) fn profil_qui_ecoute(&self, etat: &ZoneState) -> i64 {
        crate::db::hidden_repo::profil_des_bannis_de_la_zone(&self.db, etat.session_profile_id)
    }

    /// Le bail du profil qui écoute sur la zone, à `maintenant`.
    pub(super) fn bail_de_la_zone(
        &self,
        etat: &ZoneState,
        maintenant: NaiveDateTime,
    ) -> Result<Option<Bail>, Refus> {
        bail_d_ecoute::demander(&self.db, self.profil_qui_ecoute(etat), maintenant)
    }

    /// La garde de départ : `Err` porte la sentinelle `parental_lease_refused:`.
    /// Publique pour les routes qui jouent sans passer par `play()` (épisode
    /// de podcast sur une sortie réseau).
    pub async fn verifier_le_bail(&self, zone_id: i64) -> Result<(), String> {
        let etat = self.playback.get_state(zone_id).await;
        match self.bail_de_la_zone(&etat, bail_d_ecoute::maintenant_local()) {
            Ok(_) => Ok(()),
            Err(refus) => {
                tracing::info!(
                    zone_id,
                    profile_id = self.profil_qui_ecoute(&etat),
                    raison = refus.code(),
                    "parental_lease_refused"
                );
                Err(refus.sentinelle())
            }
        }
    }

    /// L'enchaînement sans blanc d'une piste qui commencera dans
    /// `debut_dans` est-il couvert par le bail ? Faux si le bail est refusé
    /// ou s'il finit avant : la piste n'est alors pas armée sur le renderer,
    /// et la fin de piste passe par la garde de départ.
    pub(crate) async fn bail_couvre_l_enchainement(
        &self,
        zone_id: i64,
        debut_dans: Duration,
    ) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        // `reste_s` borne déjà la plage ET le budget.
        let couvre = match self.bail_de_la_zone(&etat, bail_d_ecoute::maintenant_local()) {
            Ok(None) | Ok(Some(Bail { reste_s: None })) => true,
            Ok(Some(Bail { reste_s: Some(r) })) => r > debut_dans.as_secs() as i64,
            Err(_) => false,
        };
        if !couvre {
            tracing::info!(zone_id, "parental_lease_next_not_armed");
        }
        couvre
    }

    /// Le décompte et la fin de bail d'une zone, à chaque tour du sondeur.
    pub async fn decompter_le_bail(&self, etat: &ZoneState) {
        self.decompter_le_bail_a(etat, Instant::now(), bail_d_ecoute::maintenant_local())
            .await
    }

    /// Corps de [`Self::decompter_le_bail`], horloges injectées.
    pub async fn decompter_le_bail_a(
        &self,
        etat: &ZoneState,
        maintenant: Instant,
        heure_locale: NaiveDateTime,
    ) {
        let zone_id = etat.zone_id;
        if etat.state != PlayState::Playing {
            // Pause ou arrêt : le temps écoulé depuis le dernier tour n'est
            // pas compté, le reste déjà compté s'écrit.
            let suivi = self
                .suivis_de_bail
                .lock()
                .ok()
                .and_then(|mut s| s.remove(&zone_id));
            if let Some(s) = suivi.filter(|s| s.supervise) {
                self.crediter_le_temps_ecoute(s.profile_id, heure_locale, s.ms_non_ecrites);
            }
            return;
        }
        let profile_id = self.profil_qui_ecoute(etat);
        let precedent = self
            .suivis_de_bail
            .lock()
            .ok()
            .and_then(|s| s.get(&zone_id).cloned())
            .filter(|s| s.profile_id == profile_id);
        let mut suivi = match precedent {
            Some(mut s) => {
                if s.supervise {
                    let ecart = maintenant
                        .saturating_duration_since(s.dernier_tour)
                        .as_millis() as i64;
                    s.ms_non_ecrites += ecart.min(ECART_MAX_MS);
                }
                s.dernier_tour = maintenant;
                s
            }
            None => {
                // Premier tour en lecture : la politique se lit une fois.
                let supervise = matches!(
                    bail_d_ecoute::politique_limitante(&self.db, profile_id),
                    Ok(Some(_)) | Err(_)
                );
                let mut s = SuiviDuBail {
                    profile_id,
                    supervise,
                    dernier_tour: maintenant,
                    ms_non_ecrites: 0,
                    fin: None,
                    arret_a: None,
                };
                if supervise {
                    s.fin = self.fin_du_bail(profile_id, maintenant, heure_locale);
                }
                s
            }
        };
        if !suivi.supervise {
            self.ranger_le_suivi(zone_id, suivi);
            return;
        }
        if suivi.ms_non_ecrites >= PAS_D_ECRITURE_MS {
            let secondes = suivi.ms_non_ecrites / 1000;
            self.crediter_le_temps_ecoute(profile_id, heure_locale, secondes * 1000);
            suivi.ms_non_ecrites -= secondes * 1000;
            // Le compteur a bougé, la politique a pu changer : on relit.
            if suivi.arret_a.is_none() {
                suivi.fin = self.fin_du_bail(profile_id, maintenant, heure_locale);
            }
        }
        let reste_piste_ms = etat
            .now_playing
            .as_ref()
            .filter(|np| np.duration_ms > 0)
            .map(|np| np.duration_ms - etat.position_ms);
        match geste_de_fin_de_bail(maintenant, suivi.fin, suivi.arret_a, reste_piste_ms) {
            GesteDeFinDeBail::Rien | GesteDeFinDeBail::LaisserFinirLaPiste => {
                self.ranger_le_suivi(zone_id, suivi);
            }
            GesteDeFinDeBail::Annoncer => {
                suivi.arret_a = Some(maintenant + DELAI_D_ANNONCE);
                tracing::info!(zone_id, profile_id, "parental_lease_ending");
                self.playback.emit_bail_parental(
                    zone_id,
                    "parental_lease_ending",
                    serde_json::json!({
                        "code": bail_d_ecoute::CODE_REFUS,
                        "stop_in_s": DELAI_D_ANNONCE.as_secs(),
                    }),
                );
                self.ranger_le_suivi(zone_id, suivi);
            }
            GesteDeFinDeBail::Arreter => {
                self.crediter_le_temps_ecoute(profile_id, heure_locale, suivi.ms_non_ecrites);
                if let Ok(mut s) = self.suivis_de_bail.lock() {
                    s.remove(&zone_id);
                }
                tracing::info!(zone_id, profile_id, "parental_lease_ended");
                self.stop(zone_id, None).await;
                self.playback.emit_bail_parental(
                    zone_id,
                    "parental_lease_ended",
                    serde_json::json!({ "code": bail_d_ecoute::CODE_REFUS }),
                );
            }
        }
    }

    fn ranger_le_suivi(&self, zone_id: i64, suivi: SuiviDuBail) {
        if let Ok(mut s) = self.suivis_de_bail.lock() {
            s.insert(zone_id, suivi);
        }
    }

    /// L'instant de fin du bail : maintenant s'il est déjà refusé, `None`
    /// s'il n'a pas de fin.
    fn fin_du_bail(
        &self,
        profile_id: i64,
        maintenant: Instant,
        heure_locale: NaiveDateTime,
    ) -> Option<Instant> {
        match bail_d_ecoute::demander(&self.db, profile_id, heure_locale) {
            Ok(None) | Ok(Some(Bail { reste_s: None })) => None,
            Ok(Some(Bail { reste_s: Some(r) })) => {
                Some(maintenant + Duration::from_secs(r.max(0) as u64))
            }
            Err(_) => Some(maintenant),
        }
    }

    fn crediter_le_temps_ecoute(&self, profile_id: i64, heure_locale: NaiveDateTime, ms: i64) {
        if let Err(e) =
            bail_d_ecoute::crediter(&self.db, profile_id, heure_locale.date(), ms / 1000)
        {
            tracing::warn!(profile_id, error = %e, "parental_listening_time_not_written");
        }
    }
}
