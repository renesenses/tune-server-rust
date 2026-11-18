//! TESS T17 — appliquer une chaîne chargée (préréglage) au son, selon le
//! mode d'application de la maquette A2.
//!
//! - **`at_next_playback`** (et toute valeur inconnue) : rien ne bouge ; la
//!   prochaine lecture part avec la chaîne écrite.
//! - **`immediately`**, sortie **locale** : les moteurs sont remplacés EN
//!   VOL par `replace_*_live` (égaliseur, crossfeed, repli mono,
//!   réaffectation : historique des filtres conservé, rampes de #5215),
//!   via [`PlaybackOrchestrator::refresh_zone_pure_dsp`] ; le convolueur et
//!   le suréchantillonneur sont reposés. Aucun arrêt du flux, donc **aucun
//!   clic** dû à une relance.
//! - **`immediately`**, zone **réseau** qui joue : une relance à la position
//!   EN COURS (#8422, [`position_a_reprendre`]), **regroupée** par la
//!   [`FileDeRelance`] : une rafale de chargements donne UNE relance après
//!   l'anti-rebond, puis au plus une par plancher ; la dernière demande n'est
//!   jamais perdue (le plancher RETARDE, il n'abandonne pas, contrairement à
//!   `schedule_eq_replay`). Une zone en pause ou arrêtée ne repart jamais
//!   (#7556) ; une position que personne ne mesure ne se relance pas (#2595).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::PlaybackOrchestrator;
use super::dsp::position_a_reprendre;
use crate::tess::ModeApplication;

/// Anti-rebond d'une rafale (même valeur que `EQ_REPLAY_DEBOUNCE_MS`).
pub const ANTI_REBOND: Duration = Duration::from_millis(500);
/// Écart minimal entre deux relances d'une zone (même valeur que
/// `EQ_REPLAY_FLOOR_MS`).
pub const PLANCHER: Duration = Duration::from_millis(5_000);

/// Ce que l'application a fait (publié dans la réponse : `application`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GesteTess {
    /// Mode « At next playback » : rien n'est touché.
    AuProchainDemarrage,
    /// Rien ne joue : la prochaine lecture part avec la chaîne.
    RienNeJoue,
    /// Zone en pause : rien ne repart ; la chaîne s'appliquera à la reprise
    /// de la lecture suivante.
    EnPause,
    /// Sortie locale : moteurs remplacés en vol, sans arrêt du flux.
    AChaud,
    /// Zone réseau : relance à la position programmée (ou regroupée avec une
    /// relance déjà programmée).
    Relance,
    /// Zone réseau dont personne ne mesure la position : pas de relance
    /// (#2595), la chaîne s'appliquera à la piste suivante.
    PositionInconnue,
}

impl GesteTess {
    pub fn code(self) -> &'static str {
        match self {
            Self::AuProchainDemarrage => "at_next_playback",
            Self::RienNeJoue => "not_playing",
            Self::EnPause => "paused",
            Self::AChaud => "applied_live",
            Self::Relance => "restart_at_position",
            Self::PositionInconnue => "next_track",
        }
    }
}

/// La décision, isolée de toute sortie (éprouvable sans audio).
pub fn decider(
    mode: ModeApplication,
    etat: crate::playback::PlayState,
    sortie_locale: bool,
    position_mesuree: bool,
) -> GesteTess {
    use crate::playback::PlayState;
    if mode != ModeApplication::Immediately {
        return GesteTess::AuProchainDemarrage;
    }
    match etat {
        PlayState::Stopped => GesteTess::RienNeJoue,
        _ if sortie_locale => GesteTess::AChaud,
        PlayState::Playing if position_mesuree => GesteTess::Relance,
        PlayState::Playing => GesteTess::PositionInconnue,
        _ => GesteTess::EnPause,
    }
}

/// Ce que la tâche de relance d'une zone doit faire maintenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Etape {
    /// Attendre (anti-rebond ou plancher), puis redemander.
    Attendre(Duration),
    /// Relancer maintenant.
    Relancer,
    /// Plus rien à faire : la tâche se termine.
    Finir,
}

#[derive(Debug, Default)]
struct EtatZone {
    /// Une demande n'a pas encore été servie.
    sale: bool,
    /// Instant de la dernière demande (anti-rebond).
    derniere_demande: Option<Instant>,
    /// Instant de la dernière relance (plancher).
    derniere_relance: Option<Instant>,
    /// Une tâche tourne pour cette zone.
    tache: bool,
}

/// La file de relance par zone : UNE tâche par zone, une relance par
/// rafale, au plus une par [`PLANCHER`], la dernière demande toujours servie.
#[derive(Debug, Default)]
pub struct FileDeRelance {
    zones: HashMap<i64, EtatZone>,
}

impl FileDeRelance {
    /// Une demande. Rend `true` si l'appelant doit lancer la tâche de la
    /// zone ; `false` si une tâche tourne déjà (demande regroupée).
    pub fn demander(&mut self, zone_id: i64, maintenant: Instant) -> bool {
        let z = self.zones.entry(zone_id).or_default();
        z.sale = true;
        z.derniere_demande = Some(maintenant);
        if z.tache {
            false
        } else {
            z.tache = true;
            true
        }
    }

    /// Ce que la tâche doit faire à `maintenant`.
    pub fn prochaine(&mut self, zone_id: i64, maintenant: Instant) -> Etape {
        let z = self.zones.entry(zone_id).or_default();
        if !z.sale {
            z.tache = false;
            return Etape::Finir;
        }
        let rebond = z
            .derniere_demande
            .map(|t| (t + ANTI_REBOND).saturating_duration_since(maintenant))
            .unwrap_or_default();
        let plancher = z
            .derniere_relance
            .map(|t| (t + PLANCHER).saturating_duration_since(maintenant))
            .unwrap_or_default();
        let attente = rebond.max(plancher);
        if attente.is_zero() {
            z.sale = false;
            Etape::Relancer
        } else {
            Etape::Attendre(attente)
        }
    }

    /// La relance a eu lieu (réussie ou non : le plancher vaut dans les deux
    /// cas, une relance ratée a déjà coupé le son).
    pub fn relancee(&mut self, zone_id: i64, maintenant: Instant) {
        self.zones.entry(zone_id).or_default().derniere_relance = Some(maintenant);
    }

    /// La tâche abandonne (zone arrêtée, position perdue) : plus de tâche,
    /// plus de demande en attente.
    pub fn abandonner(&mut self, zone_id: i64) {
        if let Some(z) = self.zones.get_mut(&zone_id) {
            z.sale = false;
            z.tache = false;
        }
    }
}

fn file() -> &'static Mutex<FileDeRelance> {
    static FILE: OnceLock<Mutex<FileDeRelance>> = OnceLock::new();
    FILE.get_or_init(|| Mutex::new(FileDeRelance::default()))
}

impl PlaybackOrchestrator {
    /// Applique au son la chaîne qui vient d'être écrite pour la zone (voir
    /// le module).
    pub async fn appliquer_la_chaine_tess(
        self: &Arc<Self>,
        zone_id: i64,
        mode: ModeApplication,
    ) -> GesteTess {
        let etat = self.playback.get_state(zone_id).await;
        let device = self.peripherique_tess(zone_id);
        let locale = device.starts_with("local:");
        let geste = decider(
            mode,
            etat.state,
            locale,
            locale || self.position_entretenue_par_le_sondeur(zone_id),
        );
        match geste {
            GesteTess::AChaud => self.appliquer_en_vol(zone_id).await,
            GesteTess::Relance => self.demander_une_relance_tess(zone_id),
            _ => {}
        }
        info!(zone_id, geste = geste.code(), "tess_chaine_appliquee");
        geste
    }

    fn peripherique_tess(&self, zone_id: i64) -> String {
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
            .unwrap_or_default()
    }

    /// Sortie locale : tout repousser en vol, sans arrêter le flux.
    async fn appliquer_en_vol(self: &Arc<Self>, zone_id: i64) {
        self.refresh_zone_pure_dsp(zone_id).await;
        self.refresh_zone_upsampler(zone_id).await;
        #[cfg(feature = "local-audio")]
        {
            let device = self.peripherique_tess(zone_id);
            let ir = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
                .get(&format!("ir_path_{zone_id}"))
                .ok()
                .flatten()
                .filter(|_| self.correction_ir_autorisee() && !self.zone_audiophile(zone_id));
            let Some(arc) = ({ self.outputs.lock().await.get(&device) }) else {
                return;
            };
            let sortie = arc.lock().await;
            if let Some(local) = sortie
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            {
                match ir {
                    Some(chemin) => {
                        if let Err(e) = local.set_convolver_ir(&chemin) {
                            warn!(zone_id, error = %e, "tess_convolution_non_posee");
                        }
                    }
                    None => local.clear_convolver(),
                }
            }
        }
    }

    /// Zone réseau : une relance regroupée (voir [`FileDeRelance`]).
    fn demander_une_relance_tess(self: &Arc<Self>, zone_id: i64) {
        if !file().lock().unwrap().demander(zone_id, Instant::now()) {
            info!(zone_id, "tess_relance_regroupee");
            return;
        }
        let moi = self.clone();
        tokio::spawn(async move {
            loop {
                let etape = file().lock().unwrap().prochaine(zone_id, Instant::now());
                match etape {
                    Etape::Finir => return,
                    Etape::Attendre(d) => tokio::time::sleep(d).await,
                    Etape::Relancer => {
                        // #7556 — un réglage ne relance JAMAIS une zone qui
                        // ne joue plus.
                        let etat = moi.playback.get_state(zone_id).await;
                        if etat.state != crate::playback::PlayState::Playing {
                            info!(zone_id, "tess_relance_abandonnee_zone_pas_en_lecture");
                            file().lock().unwrap().abandonner(zone_id);
                            return;
                        }
                        let position_ms = position_a_reprendre(&etat);
                        let r = moi
                            .replay_zone_at_position(zone_id, position_ms, "tess_preset")
                            .await;
                        let maintenant = Instant::now();
                        file().lock().unwrap().relancee(zone_id, maintenant);
                        // Le plancher de l'égaliseur voit aussi cette relance.
                        moi.eq_replay_last
                            .lock()
                            .unwrap()
                            .insert(zone_id, maintenant);
                        match r {
                            Ok(()) => info!(zone_id, position_ms, "tess_relance_faite"),
                            Err(e) => warn!(zone_id, error = %e, "tess_relance_echouee"),
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
#[path = "relance_tess_t17_tests.rs"]
mod tests;
