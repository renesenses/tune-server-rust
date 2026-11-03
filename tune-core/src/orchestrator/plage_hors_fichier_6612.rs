//! #6612 — une plage HORS DU FICHIER à la reprise : nouveau flux, à la position.
//!
//! Fil 2346 (Cyrille, 1.0.0-rc6) : Yamaha R-N2000A en DLNA, Qobuz relayé par
//! le mandataire. À la pause, le Yamaha ferme sa connexion ; au `Play`, il la
//! rouvre avec `Range: bytes=4294949906-` (2^32 − 17 390) pour un fichier de
//! 75,7 Mo. La plage partait au CDN, qui répondait 416, renvoyé tel quel ; le
//! renderer restait à l'arrêt, sans un son, et seul « Suivant » — un nouveau
//! flux — le relançait.
//!
//! Décision de Bertrand (20/10) : une plage qui commence au-delà de la
//! longueur connue ne part pas au CDN, et Tune reprend par un NOUVEAU flux à
//! la position mémorisée. Trois temps :
//!
//! 1. `tune-stream-http` rend le 416 sans requête amont et le note sur la
//!    session (`StreamSession::signaler_plage_hors_fichier`) — voie
//!    mandataire comme voie fichier, qui avait le même trou ;
//! 2. `resume` retient chaque reprise SUR PLACE d'un renderer réseau, avec sa
//!    position ([`PlaybackOrchestrator::noter_reprise_sur_place`]) ;
//! 3. le sondeur, qui voit la plage notée et le renderer qui ne joue pas
//!    ([`suite_de_plage_hors_fichier`]), fait reprendre la zone par
//!    [`PlaybackOrchestrator::reprendre_apres_plage_hors_fichier`] : le chemin
//!    « rétablir à la position » de `resume` (nouvelle URI, position, `Seek`
//!    ou départ natif), celui de #6582 et d'une session morte.
//!
//! Une plage hors fichier sans reprise récente (une sonde, une fin de piste)
//! ne relance rien : le 416 reste la seule réponse, comme avant.

use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::PlaybackOrchestrator;
use crate::outputs::TransportState;

/// Une plage hors fichier ne relance la zone que dans cette fenêtre après une
/// reprise sur place : c'est le cas du Yamaha, qui rouvre au `Play`.
pub(crate) const FENETRE_APRES_REPRISE: Duration = Duration::from_secs(60);

/// Au-delà, la plage notée est oubliée sans rien relancer.
pub(crate) const PEREMPTION_DU_SIGNAL: Duration = Duration::from_secs(30);

/// Un renderer en `TRANSITIONING` a ce délai pour se décider.
pub(crate) const PATIENCE_EN_TRANSITION: Duration = Duration::from_secs(5);

/// Ce que le sondeur fait d'une plage hors fichier notée sur le flux en cours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuiteDePlageHorsFichier {
    /// Aucune plage hors fichier n'est notée.
    Rien,
    /// Le renderer joue encore, ou transite : on revoit au tour suivant.
    Attendre,
    /// La plage est trop ancienne : on l'oublie, sans rien relancer.
    Oublier,
    /// Le renderer s'est arrêté après la plage hors fichier : nouveau flux.
    Reprendre,
}

/// Fonction PURE : `age` est l'âge de la plage notée (`None` = aucune),
/// `etat` celui que le renderer vient de rendre.
pub(crate) fn suite_de_plage_hors_fichier(
    age: Option<Duration>,
    etat: TransportState,
) -> SuiteDePlageHorsFichier {
    let Some(age) = age else {
        return SuiteDePlageHorsFichier::Rien;
    };
    if age > PEREMPTION_DU_SIGNAL {
        return SuiteDePlageHorsFichier::Oublier;
    }
    match etat {
        // Un renderer qui joue n'est pas à relancer ; s'il s'arrête avant la
        // péremption, le tour suivant le verra.
        TransportState::Playing => SuiteDePlageHorsFichier::Attendre,
        TransportState::Transitioning if age < PATIENCE_EN_TRANSITION => {
            SuiteDePlageHorsFichier::Attendre
        }
        TransportState::Transitioning | TransportState::Stopped | TransportState::Paused => {
            SuiteDePlageHorsFichier::Reprendre
        }
    }
}

impl PlaybackOrchestrator {
    /// Retient une reprise SUR PLACE d'un renderer réseau, et sa position.
    pub(super) fn noter_reprise_sur_place(&self, zone_id: i64, position_ms: u64) {
        self.reprises_sur_place
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(zone_id, (position_ms, Instant::now()));
    }

    /// #6612 — le renderer a rouvert le flux par une plage hors fichier juste
    /// après une reprise sur place, et s'est arrêté : reprendre par un NOUVEAU
    /// flux, à la position de cette reprise. Rend `true` si la zone a été
    /// relancée.
    ///
    /// Sans reprise sur place dans les [`FENETRE_APRES_REPRISE`] précédentes,
    /// rien n'est relancé.
    pub async fn reprendre_apres_plage_hors_fichier(&self, zone_id: i64, device_id: &str) -> bool {
        let reprise = self
            .reprises_sur_place
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&zone_id);
        let Some((position_ms, quand)) =
            reprise.filter(|(_, quand)| quand.elapsed() <= FENETRE_APRES_REPRISE)
        else {
            info!(
                zone_id,
                "plage_hors_fichier_sans_reprise_recente — rien n'est relancé"
            );
            return false;
        };
        let state = self.playback.get_state(zone_id).await;
        let Some(np) = state.now_playing.as_ref().filter(|np| np.source != "radio") else {
            return false;
        };
        info!(
            zone_id,
            position_ms,
            depuis_la_reprise_ms = quand.elapsed().as_millis() as u64,
            "plage_hors_fichier_reprise_par_nouveau_flux"
        );
        match self
            .retablir_a_la_position(zone_id, device_id, np, position_ms)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!(zone_id, position_ms, error = %e, "plage_hors_fichier_reprise_echouee");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::db::zone_repo::ZoneRepo;
    use crate::http::streamer::AudioStreamer;
    use crate::outputs::mock::MockOutput;
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::{NowPlaying, PlaybackManager};
    use crate::streaming::registry::ServiceRegistry;

    const APPAREIL: &str = "uuid:yamaha-r-n2000a";
    const POSITION_MS: i64 = 129_000;

    #[test]
    fn la_suite_depend_de_l_age_et_de_l_etat_du_renderer() {
        use SuiteDePlageHorsFichier::*;
        let s = Duration::from_secs;
        assert_eq!(
            suite_de_plage_hors_fichier(None, TransportState::Stopped),
            Rien
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(2)), TransportState::Stopped),
            Reprendre,
            "le cas du Yamaha : STOPPED juste après le 416"
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(2)), TransportState::Paused),
            Reprendre
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(2)), TransportState::Playing),
            Attendre
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(2)), TransportState::Transitioning),
            Attendre
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(6)), TransportState::Transitioning),
            Reprendre
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(31)), TransportState::Stopped),
            Oublier
        );
        assert_eq!(
            suite_de_plage_hors_fichier(Some(s(31)), TransportState::Playing),
            Oublier
        );
    }

    fn ecrire_wav(chemin: &std::path::Path) {
        let octets_data: u32 = 44_100 * 4;
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + octets_data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&octets_data.to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, octets_data as usize));
        std::fs::write(chemin, v).unwrap();
    }

    /// Une zone DLNA (sortie factice) qui joue une piste à `POSITION_MS`,
    /// puis la met en pause. Le dossier rendu garde le fichier en vie.
    async fn zone_en_pause() -> (PlaybackOrchestrator, i64, crate::test_scratch::ScratchDir) {
        let dossier = crate::test_scratch::scratch_dir("tune-6612-plage-hors-fichier");
        let fichier = dossier.join("piste.wav");
        ecrire_wav(&fichier);
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let orch = PlaybackOrchestrator::new(
            db.clone(),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        );
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon Yamaha", Some("dlna"), Some(APPAREIL))
            .unwrap();
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "R-N2000A").with_type("dlna"),
        ));
        let mut piste = crate::db::models::Track::new("Piste".into());
        piste.file_path = Some(fichier.to_string_lossy().into_owned());
        piste.format = Some("wav".into());
        piste.duration_ms = 300_000;
        let track_id = crate::db::track_repo::TrackRepo::with_backend(db.clone())
            .create(&piste)
            .unwrap();
        let (sid, _tx, _ready) = orch
            .streamer
            .create_session(
                crate::http::streamer::StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 44_100,
                    bit_depth: 16,
                    channels: 2,
                    ..Default::default()
                },
                false,
                4,
            )
            .await;
        orch.playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(track_id),
                    title: "Piste".into(),
                    source: "local".into(),
                    stream_id: Some(sid),
                    duration_ms: 300_000,
                    ..Default::default()
                },
            )
            .await;
        orch.playback.update_position(zone_id, POSITION_MS).await;
        orch.playback.pause(zone_id).await;
        (orch, zone_id, dossier)
    }

    /// (play_media reçus, resume reçus) par la sortie factice.
    async fn commandes(orch: &PlaybackOrchestrator) -> (usize, u64) {
        let outputs = orch.outputs.lock().await;
        let sortie = outputs.get(APPAREIL).unwrap();
        let sortie = sortie.lock().await;
        let mock = sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .expect("la sortie factice");
        (mock.play_call_count().await, mock.resume_call_count())
    }

    /// 🔴 Le cas du fil 2346 : reprise sur place, puis plage hors fichier et
    /// renderer arrêté — la zone repart par un NOUVEAU flux, à la position.
    #[tokio::test]
    async fn apres_une_reprise_sur_place_la_plage_hors_fichier_relance_a_la_position_6612() {
        let (orch, zone_id, _dossier) = zone_en_pause().await;
        orch.resume(zone_id, Some(APPAREIL)).await.unwrap();
        assert_eq!(
            commandes(&orch).await,
            (0, 1),
            "d'abord la reprise sur place"
        );

        assert!(
            orch.reprendre_apres_plage_hors_fichier(zone_id, APPAREIL)
                .await,
            "#6612 : la zone doit être relancée"
        );
        assert_eq!(
            commandes(&orch).await,
            (1, 1),
            "#6612 : un NOUVEAU flux (play_media), pas un second Play sur place"
        );
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(
            state.position_ms, POSITION_MS,
            "#6612 : la reprise se fait à la position mémorisée"
        );
        assert!(
            !orch
                .reprendre_apres_plage_hors_fichier(zone_id, APPAREIL)
                .await,
            "une seule relance par reprise"
        );
    }

    /// Sans reprise sur place récente (une sonde, une fin de piste) : rien.
    #[tokio::test]
    async fn sans_reprise_recente_rien_n_est_relance() {
        let (orch, zone_id, _dossier) = zone_en_pause().await;
        assert!(
            !orch
                .reprendre_apres_plage_hors_fichier(zone_id, APPAREIL)
                .await
        );
        assert_eq!(commandes(&orch).await, (0, 0));

        orch.resume(zone_id, Some(APPAREIL)).await.unwrap();
        orch.reprises_sur_place
            .lock()
            .unwrap()
            .get_mut(&zone_id)
            .expect("la reprise sur place est retenue")
            .1 -= FENETRE_APRES_REPRISE + Duration::from_secs(1);
        assert!(
            !orch
                .reprendre_apres_plage_hors_fichier(zone_id, APPAREIL)
                .await,
            "hors de la fenêtre après la reprise : rien"
        );
        assert_eq!(commandes(&orch).await, (0, 1));
    }

    /// Garde de BRANCHEMENT : le sondeur relit la plage notée et appelle la
    /// reprise par nouveau flux. Compté hors définitions, dans le code de
    /// production de `tick.rs`.
    #[test]
    fn le_sondeur_branche_la_reprise_apres_plage_hors_fichier() {
        let src = include_str!("../poller/tick.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        for appel in [
            "suite_de_plage_hors_fichier(",
            ".age_plage_hors_fichier(",
            ".reprendre_apres_plage_hors_fichier(",
        ] {
            assert!(prod.contains(appel), "tick.rs doit appeler {appel}");
        }
    }
}
