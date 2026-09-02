use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::traits::{OutputStatus, PlayMedia, TransportState};

const PREBUFFER_THRESHOLD_MS: u64 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GaplessState {
    Idle,
    Monitoring,
    Ready,
}

pub struct GaplessHandler {
    state: Mutex<GaplessState>,
    next_url_set: Mutex<bool>,
    enabled: Mutex<bool>,
    prebuffer_threshold_ms: u64,
}

impl GaplessHandler {
    pub fn new(enabled: bool) -> Self {
        Self {
            state: Mutex::new(GaplessState::Idle),
            next_url_set: Mutex::new(false),
            enabled: Mutex::new(enabled),
            prebuffer_threshold_ms: PREBUFFER_THRESHOLD_MS,
        }
    }

    pub async fn set_enabled(&self, enabled: bool) {
        *self.enabled.lock().await = enabled;
        if !enabled {
            self.reset().await;
        }
    }

    pub async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    pub async fn on_play_start(&self) {
        if !*self.enabled.lock().await {
            return;
        }
        *self.state.lock().await = GaplessState::Monitoring;
        *self.next_url_set.lock().await = false;
        debug!("gapless_monitoring_start");
    }

    pub async fn check_prebuffer(
        &self,
        status: &OutputStatus,
        zone_id: i64,
        queue_position: i64,
        queue_length: i64,
        orchestrator: &PlaybackOrchestrator,
        device_id: &str,
    ) -> bool {
        if !*self.enabled.lock().await {
            return false;
        }

        let current_state = self.state.lock().await.clone();
        if current_state != GaplessState::Monitoring {
            return false;
        }

        if status.state != TransportState::Playing || status.duration_ms == 0 {
            return false;
        }

        let remaining_ms = status.duration_ms.saturating_sub(status.position_ms);
        if remaining_ms > self.prebuffer_threshold_ms {
            return false;
        }

        if *self.next_url_set.lock().await {
            return false;
        }

        let next_pos = queue_position + 1;
        if next_pos >= queue_length {
            return false;
        }

        match orchestrator.resolve_queue_item_url(zone_id, next_pos).await {
            Ok(resolved) => {
                let outputs = orchestrator.outputs.lock().await;
                if let Some(output) = outputs.get(device_id) {
                    let out = output.lock().await;
                    let media = PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: None,
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                        live_stream: false,
                        byte_seekable: true,
                        // A staged queue item is a finite track: only the live
                        // and proxy paths have an upstream URL to carry.
                        origin_url: None,
                        source: resolved.source.as_deref(),
                        source_id: resolved.source_id.as_deref(),
                        track_number: resolved.track_number,
                        disc_number: resolved.disc_number,
                    };
                    match out.set_next_media(&media).await {
                        Ok(()) => {
                            *self.state.lock().await = GaplessState::Ready;
                            *self.next_url_set.lock().await = true;
                            info!(
                                zone_id,
                                next_pos,
                                title = ?resolved.title,
                                "gapless_next_url_set"
                            );
                            return true;
                        }
                        Err(e) => {
                            debug!(error = %e, "gapless_set_next_failed");
                        }
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "gapless_resolve_failed");
            }
        }

        false
    }

    pub async fn on_track_end(&self) {
        let was_ready = *self.state.lock().await == GaplessState::Ready;
        self.reset().await;
        if was_ready {
            info!("gapless_transition_complete");
        }
    }

    pub async fn reset(&self) {
        *self.state.lock().await = GaplessState::Idle;
        *self.next_url_set.lock().await = false;
    }

    pub async fn status(&self) -> serde_json::Value {
        let enabled = *self.enabled.lock().await;
        let state = self.state.lock().await.clone();
        let next_ready = *self.next_url_set.lock().await;
        serde_json::json!({
            "enabled": enabled,
            "state": format!("{state:?}"),
            "next_track_ready": next_ready,
            "prebuffer_threshold_ms": self.prebuffer_threshold_ms,
        })
    }
}

/// Un marqueur de format — extension, type MIME, ou `format` scanné — désigne-t-il du DSD ?
///
/// UNE seule définition, pour les trois endroits qui la posaient chacun de leur
/// côté : le garde de fin de piste DSD sur DLNA
/// (`decisions::dlna_dsd_reached_end`), le refus d'armement DSD sur DLNA
/// (`prepare_gapless`, #402) et la promesse faite à la file
/// ([`enchainement_sans_blanc`]). Deux copies, c'est la porte ouverte à ce que
/// la file promette ce que le poller refuse ensuite.
pub fn est_dsd(marqueur: &str) -> bool {
    let m = marqueur.to_lowercase();
    m.contains("dsd") || m.contains("dsf") || m.contains("dff")
}

/// Ce qu'une ligne de file peut promettre à la ligne **affichée** juste après elle.
///
/// C'est la question exacte que pose le badge « Gapless » de la file : entre
/// CETTE ligne et la SUIVANTE À L'ÉCRAN, y aura-t-il un blanc ?
///
/// Le badge n'a jamais rien affiché, pour personne, sur aucune zone : le champ
/// `gapless_next` que le client lit n'existait dans **aucune** structure
/// sérialisée du serveur — il ne survivait que dans `docs/contrat-web.json`,
/// hérité du serveur Python (Jean Valjean, fil 631, 15/06/2026, #2934). Il est
/// calculé ici à partir des MÊMES refus que le poller applique au moment
/// d'armer, jamais d'un défaut constant : un indicateur qui ment est pire que
/// pas d'indicateur.
#[derive(Debug, Clone, Copy)]
pub struct EnchainementAffiche<'a> {
    /// Réglage `zones.gapless_enabled` de la zone.
    pub gapless_enabled: bool,
    /// `capabilities().can_gapless` de la sortie réellement enregistrée.
    ///
    /// `false` couvre aussi la sortie inconnue (zone navigateur, sortie
    /// disparue) : on ne promet pas ce qu'on ne peut pas constater.
    pub output_can_gapless: bool,
    /// `output_type()` de la zone (`"dlna"`, `"local"`, `"oaat"`…).
    pub output_type: &'a str,
    /// `prefers_local_file_gapless()` : la sortie ne sait mettre en attente
    /// qu'un FICHIER local (OAAT en DSD natif ou en PCM direct).
    pub output_prefers_local_file: bool,
    /// Index AFFICHÉ de la ligne examinée.
    pub index: i64,
    /// Index que la file désignera réellement comme suivant quand cette ligne
    /// se terminera — [`crate::poller::PositionPoller::next_position_after`],
    /// la seule décision « piste suivante » du serveur. Sous aléatoire ou sous
    /// répétition-une il n'est PAS `index + 1` : l'ordre affiché ne dit alors
    /// rien de l'ordre joué.
    pub successeur_reel: Option<i64>,
    /// `format` de la ligne affichée juste après.
    pub successeur_format: Option<&'a str>,
    /// La ligne affichée juste après a-t-elle un fichier local ?
    pub successeur_est_fichier_local: bool,
}

/// Vrai **seulement** si l'enchaînement vers la ligne affichée suivante se fera
/// réellement sans blanc.
///
/// Chaque refus reproduit un refus que le poller applique déjà, et le renvoie à
/// son journal :
///
/// | refus | journal du poller |
/// |---|---|
/// | l'ordre affiché n'est pas l'ordre joué | — (aléatoire / répétition-une) |
/// | zone `gapless_enabled = 0` | le poller n'entre pas dans la branche d'armement |
/// | sortie qui ne chaîne pas depuis sa boucle | `gapless_skipped_exclusive_output` |
/// | suivant DSD sur DLNA (#402) | `gapless_skipped_dsd_next_dlna` |
/// | suivant sans fichier local sur OAAT DSD natif | `gapless_local_file_skipped_no_local_next` |
pub fn enchainement_sans_blanc(i: &EnchainementAffiche<'_>) -> bool {
    // L'ordre affiché doit ÊTRE l'ordre joué. Sous aléatoire, la ligne d'en
    // dessous n'est pas celle qui va suivre ; sous répétition-une, c'est la
    // même qui repart. Promettre là serait mentir sur QUI s'enchaîne — la
    // dernière ligne d'une file, qui n'a pas de suivant, tombe ici aussi.
    if i.successeur_reel != Some(i.index + 1) {
        return false;
    }
    if !i.gapless_enabled {
        return false;
    }
    // Sortie qui ne sait pas chaîner depuis sa propre boucle de lecture :
    // Chromecast, slimproto, AirPlay, Squeezebox, HQPlayer, sortie locale en
    // mode exclusif (ASIO / WASAPI exclusif), et sortie locale ou OAAT dont la
    // chaîne s'est épuisée.
    if !i.output_can_gapless {
        return false;
    }
    // #402 — un renderer DLNA accepte `SetNextAVTransportURI` pour un flux DSD
    // et ne le consomme jamais (HiFi Rose RS130, Benjithom). Le poller refuse
    // d'armer ; la file ne le promet donc pas. Le même DSD sur une sortie
    // locale garde sa chaîne interne et reste promis.
    if i.output_type == "dlna" && i.successeur_format.is_some_and(est_dsd) {
        return false;
    }
    // OAAT en DSD natif / PCM direct lit le `.dsf` suivant sur le disque : un
    // suivant en streaming, sans fichier local, n'est pas armé.
    if i.output_prefers_local_file && !i.successeur_est_fichier_local {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #2934 : ce que la file promet d'une ligne à la suivante ─────────────

    fn ligne() -> EnchainementAffiche<'static> {
        EnchainementAffiche {
            gapless_enabled: true,
            output_can_gapless: true,
            output_type: "local",
            output_prefers_local_file: false,
            index: 0,
            successeur_reel: Some(1),
            successeur_format: Some("flac"),
            successeur_est_fichier_local: true,
        }
    }

    #[test]
    fn est_dsd_reconnait_les_trois_marqueurs() {
        assert!(est_dsd("dsf"));
        assert!(est_dsd("DFF"));
        assert!(est_dsd("audio/x-dsd"));
        assert!(!est_dsd("flac"));
        assert!(!est_dsd("audio/x-flac"));
        assert!(!est_dsd(""));
    }

    #[test]
    fn enchaine_quand_tout_est_reuni() {
        assert!(enchainement_sans_blanc(&ligne()));
    }

    #[test]
    fn derniere_ligne_ne_promet_rien() {
        let mut l = ligne();
        l.successeur_reel = None;
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn ordre_affiche_different_de_l_ordre_joue_ne_promet_rien() {
        // Aléatoire : la ligne d'en dessous n'est pas celle qui va suivre.
        let mut l = ligne();
        l.successeur_reel = Some(7);
        assert!(!enchainement_sans_blanc(&l));
        // Répétition-une : c'est la MÊME ligne qui repart.
        let mut l = ligne();
        l.successeur_reel = Some(0);
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn zone_sans_gapless_ne_promet_rien() {
        let mut l = ligne();
        l.gapless_enabled = false;
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn sortie_qui_ne_chaine_pas_ne_promet_rien() {
        // Chromecast, slimproto, local en mode exclusif, chaîne épuisée…
        let mut l = ligne();
        l.output_can_gapless = false;
        assert!(!enchainement_sans_blanc(&l));
    }

    #[test]
    fn dsd_sur_dlna_ne_promet_rien_mais_dsd_en_local_promet() {
        // #402 : le renderer accepte SetNext et ne le consomme jamais.
        let mut dlna = ligne();
        dlna.output_type = "dlna";
        dlna.successeur_format = Some("dsf");
        assert!(!enchainement_sans_blanc(&dlna));
        // Le refus vise le COUPLE DLNA+DSD, pas l'un des deux :
        let mut dlna_flac = ligne();
        dlna_flac.output_type = "dlna";
        assert!(enchainement_sans_blanc(&dlna_flac));
        let mut local_dsd = ligne();
        local_dsd.successeur_format = Some("dsf");
        assert!(enchainement_sans_blanc(&local_dsd));
    }

    #[test]
    fn oaat_dsd_natif_ne_promet_qu_un_suivant_local() {
        let mut streaming = ligne();
        streaming.output_type = "oaat";
        streaming.output_prefers_local_file = true;
        streaming.successeur_est_fichier_local = false;
        assert!(!enchainement_sans_blanc(&streaming));
        let mut local = streaming;
        local.successeur_est_fichier_local = true;
        assert!(enchainement_sans_blanc(&local));
    }

    #[test]
    fn un_suivant_en_streaming_reste_promis_sur_une_sortie_ordinaire() {
        // Le garde « fichier local » est propre aux sorties qui l'exigent :
        // une piste Qobuz s'enchaîne normalement sur une sortie DLNA.
        let mut l = ligne();
        l.output_type = "dlna";
        l.successeur_format = None;
        l.successeur_est_fichier_local = false;
        assert!(enchainement_sans_blanc(&l));
    }

    #[tokio::test]
    async fn starts_disabled() {
        let h = GaplessHandler::new(false);
        assert!(!h.is_enabled().await);
    }

    #[tokio::test]
    async fn enable_disable() {
        let h = GaplessHandler::new(true);
        assert!(h.is_enabled().await);
        h.set_enabled(false).await;
        assert!(!h.is_enabled().await);
    }

    #[tokio::test]
    async fn monitoring_on_play() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Monitoring);
    }

    #[tokio::test]
    async fn reset_clears_state() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        h.reset().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Idle);
    }

    #[tokio::test]
    async fn status_json() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        let s = h.status().await;
        assert_eq!(s["enabled"], true);
        assert_eq!(s["state"], "Monitoring");
        assert_eq!(s["next_track_ready"], false);
    }

    #[tokio::test]
    async fn no_monitoring_when_disabled() {
        let h = GaplessHandler::new(false);
        h.on_play_start().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Idle);
    }
}
