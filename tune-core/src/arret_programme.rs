//! Arrêt programmé de la lecture à heure fixe (#8213).
//!
//! laguespa (fil 2720, Premium) veut s'endormir en musique et que Tune
//! s'arrête de lui-même « à une heure fixe tous les jours ou certains jours
//! de la semaine ». La minuterie de sommeil existante compte une DURÉE
//! (`/zones/{id}/sleep`) ; il la faut relancer chaque soir. Ce module ajoute
//! une ÉCHÉANCE : un réglage unique du serveur, une heure `HH:MM` et des
//! jours, qui arrête la lecture des zones visées (toutes par défaut) par le
//! même chemin que le bouton Stop (`PlaybackOrchestrator::stop`).
//!
//! Ce qui n'est PAS fait ici : éteindre la machine (`openrc-shutdown`,
//! `systemctl poweroff`). C'est une décision à part (droits, machines où Tune
//! n'est pas seul) ; le service Tune reste en marche pour la télécommande du
//! lendemain.
//!
//! Le réglage est rangé dans `settings` sous [`CLE`], en JSON.

use std::sync::Arc;

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::playback::PlayState;

/// Clé du réglage dans la table `settings`.
pub const CLE: &str = "scheduled_stop";

/// Le planificateur relit le réglage toutes les 20 s : une minute `HH:MM`
/// est toujours vue au moins deux fois, jamais manquée.
const INTERVALLE_S: u64 = 20;

/// Le réglage tel qu'il est rangé et tel que l'API le rend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArretProgramme {
    /// Actif ou non. Désactivé, l'heure et les jours sont gardés.
    pub enabled: bool,
    /// Heure locale du serveur, `HH:MM` sur 24 h.
    pub time: String,
    /// Sept caractères `0`/`1`, lundi en tête (même masque que les réveils).
    #[serde(default = "tous_les_jours")]
    pub days_of_week: String,
    /// Zones à arrêter ; absent ou `null` = toutes les zones.
    #[serde(default)]
    pub zone_ids: Option<Vec<i64>>,
}

fn tous_les_jours() -> String {
    "1111111".into()
}

impl ArretProgramme {
    /// Contrôle strict à l'enregistrement : un réglage invalide est refusé
    /// quand on le pose, jamais découvert à l'heure dite.
    pub fn valider(mut self) -> Result<Self, String> {
        self.time = self.time.trim().to_string();
        if heure_minute(&self.time).is_none() {
            return Err(format!("time {:?} is not HH:MM (00:00-23:59)", self.time));
        }
        if !crate::alarms::is_days_mask(&self.days_of_week) {
            return Err(format!(
                "days_of_week {:?} is not a 7-char 0/1 mask (Mon..Sun)",
                self.days_of_week
            ));
        }
        if let Some(zones) = &self.zone_ids {
            if zones.is_empty() {
                return Err("zone_ids is empty (omit it to target every zone)".into());
            }
            if zones.iter().any(|z| *z <= 0) {
                return Err("zone_ids must be positive".into());
            }
        }
        Ok(self)
    }

    /// `true` à la minute exacte de l'échéance, un jour coché, réglage actif.
    pub fn echeance(&self, maintenant: NaiveDateTime) -> bool {
        if !self.enabled {
            return false;
        }
        let Some((h, m)) = heure_minute(&self.time) else {
            return false;
        };
        jour_coche(
            &self.days_of_week,
            maintenant.weekday().num_days_from_monday(),
        ) && maintenant.hour() == h
            && maintenant.minute() == m
    }

    /// La prochaine échéance strictement après `maintenant` (dans les 8 jours),
    /// pour l'affichage « prochain arrêt ». `None` si inactif ou sans jour.
    pub fn prochaine_echeance(&self, maintenant: NaiveDateTime) -> Option<NaiveDateTime> {
        if !self.enabled {
            return None;
        }
        let (h, m) = heure_minute(&self.time)?;
        (0..=7).find_map(|decalage| {
            let jour = maintenant.date() + chrono::Duration::days(decalage);
            let quand = jour.and_hms_opt(h, m, 0)?;
            (quand > maintenant
                && jour_coche(&self.days_of_week, jour.weekday().num_days_from_monday()))
            .then_some(quand)
        })
    }
}

/// `HH:MM` → (heure, minute), bornes comprises.
fn heure_minute(texte: &str) -> Option<(u32, u32)> {
    let (h, m) = texte.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

fn jour_coche(masque: &str, jour: u32) -> bool {
    masque.as_bytes().get(jour as usize) == Some(&b'1')
}

/// Lit le réglage. Absent ou illisible (journalisé) → `None`.
pub fn lire(db: &Arc<dyn DbBackend>) -> Option<ArretProgramme> {
    let brut = SettingsRepo::with_backend(db.clone())
        .get_sans_ecrivain(CLE)
        .ok()
        .flatten()?;
    match serde_json::from_str::<ArretProgramme>(&brut) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(error = %e, "scheduled_stop_unreadable");
            None
        }
    }
}

/// Valide puis range le réglage ; rend la version rangée.
pub fn ecrire(db: &Arc<dyn DbBackend>, reglage: ArretProgramme) -> Result<ArretProgramme, String> {
    let reglage = reglage.valider()?;
    let json = serde_json::to_string(&reglage).map_err(|e| e.to_string())?;
    SettingsRepo::with_backend(db.clone()).set(CLE, &json)?;
    Ok(reglage)
}

/// Retire le réglage.
pub fn effacer(db: &Arc<dyn DbBackend>) -> Result<(), String> {
    SettingsRepo::with_backend(db.clone()).delete(CLE)
}

/// Le planificateur : une tâche de fond, un déclenchement par échéance.
pub struct PlanificateurArretProgramme {
    db: Arc<dyn DbBackend>,
    orchestrator: Arc<PlaybackOrchestrator>,
}

impl PlanificateurArretProgramme {
    pub fn new(db: Arc<dyn DbBackend>, orchestrator: Arc<PlaybackOrchestrator>) -> Self {
        Self { db, orchestrator }
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("scheduled_stop_scheduler_started");
            // La minute déjà servie (« AAAA-MM-JJ HH:MM ») : une seule
            // exécution par échéance malgré les deux passages par minute.
            let mut derniere: Option<String> = None;
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(INTERVALLE_S));
            loop {
                ticker.tick().await;
                let maintenant = chrono::Local::now().naive_local();
                self.passage(maintenant, &mut derniere).await;
            }
        })
    }

    /// Un passage du planificateur à l'instant `maintenant`. Rend les zones
    /// arrêtées (vide hors échéance ou si l'échéance est déjà servie).
    pub async fn passage(
        &self,
        maintenant: NaiveDateTime,
        derniere: &mut Option<String>,
    ) -> Vec<i64> {
        let Some(reglage) = lire(&self.db) else {
            return Vec::new();
        };
        if !reglage.echeance(maintenant) {
            return Vec::new();
        }
        let minute = maintenant.format("%Y-%m-%d %H:%M").to_string();
        if derniere.as_deref() == Some(minute.as_str()) {
            return Vec::new();
        }
        *derniere = Some(minute);
        self.arreter(&reglage).await
    }

    /// Arrête les zones visées qui jouent ou sont en pause.
    async fn arreter(&self, reglage: &ArretProgramme) -> Vec<i64> {
        let zones: Vec<i64> = match &reglage.zone_ids {
            Some(ids) => ids.clone(),
            None => crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|z| z.id)
                .collect(),
        };
        let mut arretees = Vec::new();
        for zone_id in zones {
            if self.orchestrator.playback.get_state(zone_id).await.state == PlayState::Stopped {
                continue;
            }
            self.orchestrator.stop(zone_id, None).await;
            arretees.push(zone_id);
        }
        info!(time = %reglage.time, zones = ?arretees, "scheduled_stop_triggered");
        arretees
    }
}

#[cfg(test)]
mod tests_8213;
