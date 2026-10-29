//! #5050 (fil 2236, FabienM, 12/10/2026) — le transfert d'une zone EN PAUSE
//! vers un appareil.
//!
//! Le journal : la zone Chromecast « Enfants », en pause, est transférée vers
//! « Parents » (Beosound Stage, DLNA). Le transfert lançait la piste sur la
//! cible (`SetAVTransportURI`, `Play`), la repositionnait (`Seek`), puis la
//! remettait en pause (`Pause`) — trois ordres en moins d'une seconde.
//! La Pause tombait pendant le chargement : 701 en `STOPPED`, puis 701 en
//! `TRANSITIONING`, et le renderer n'est plus jamais sorti de sa
//! transition — aucun son, position figée à 0:17, chaque Pause refusée.
//!
//! Le choix : quand la source est en pause, la cible n'est plus LANCÉE. Elle
//! est posée en pause, sur la même piste et à la même position, sans aucun
//! ordre envoyé au renderer. La reprise suivante passe par le rétablissement
//! à la position (`RetablirALaPosition` de `reprendre`) : un seul
//! `SetAVTransportURI` + `Play`, puis le `Seek` d'après relecture. C'est le
//! chemin, déjà éprouvé, d'une session ramassée pendant une longue pause.
//!
//! Pourquoi pas `SetAVTransportURI` + `Seek` sans `Play` : un `Seek` en
//! `STOPPED` est facultatif dans le contrat AVTransport, et un renderer qui
//! le refuse (ou l'ignore) repartirait de zéro à la reprise ; il faudrait
//! aussi un chemin de lecture « chargé sans jouer » qui n'existe dans aucune
//! des sorties. Ne rien envoyer ne dépend d'aucun renderer.
//!
//! La piste doit pouvoir être redemandée (`track_id` ou `source_id`) : sinon
//! le rétablissement n'aurait rien à rejouer, et le transfert garde
//! l'ancienne conduite.
use super::PlaybackOrchestrator;
use crate::outputs::OutputCommandError;
use crate::playback::{NowPlaying, PlayState};
use tracing::{info, warn};

/// Le lancement de la cible peut échouer avant une commande de transport.
/// Le distinguer garde les refus Pause/Seek existants sans inventer une
/// commande « Play » dans le contrat des sorties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurDeTransfert {
    Lancement(String),
    Commande(OutputCommandError),
}

impl From<OutputCommandError> for ErreurDeTransfert {
    fn from(erreur: OutputCommandError) -> Self {
        Self::Commande(erreur)
    }
}
impl std::fmt::Display for ErreurDeTransfert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lancement(message) => f.write_str(message),
            Self::Commande(erreur) => std::fmt::Display::fmt(erreur, f),
        }
    }
}
impl std::error::Error for ErreurDeTransfert {}

impl PlaybackOrchestrator {
    /// Lance un transfert sur l'appareil `did` de la zone cible.
    ///
    /// `queue_position` : l'index de la piste dans la file (déjà copiée).
    /// `source_position_ms` et `source_en_pause` : l'état de la source au
    /// moment du transfert. Un échec de lancement remonte à l'appelant :
    /// celui-ci ne doit ni annoncer le transfert ni arrêter sa source.
    /// Un Seek refusé garde la lecture de la cible depuis le début.
    pub async fn transferer_sur_l_appareil(
        &self,
        zone_id: i64,
        did: &str,
        queue_position: i64,
        np: &NowPlaying,
        source_position_ms: u64,
        source_en_pause: bool,
    ) -> Result<(), ErreurDeTransfert> {
        if source_en_pause
            && self
                .poser_le_transfert_en_pause(zone_id, did, np, source_position_ms)
                .await
        {
            return Ok(());
        }
        // #6470 (FabienM, fil 2302 point 16) — une RADIO ou un épisode de
        // PODCAST ne vit pas dans la file : `play_from_queue` lançait sur la
        // cible la file qu'elle avait déjà, et la cible jouait (ou affichait)
        // son dernier morceau au lieu du flux transféré. Le flux est relancé
        // tel quel sur l'appareil cible.
        let flux = np.source_id.as_deref().filter(|s| !s.is_empty());
        let direct = matches!(np.source.as_str(), "radio" | "podcast") && flux.is_some();
        // #6466 (point 15) — la piste transférée a déjà son écoute, écrite
        // avec la zone d'origine : la relancer sur la cible ne la réécrit pas.
        // Les titres suivants, eux, s'écriront avec la zone cible.
        let lancement = if direct {
            self.play_transfere_sans_historique(super::PlayRequest {
                zone_id,
                output_device_id: Some(did.to_string()),
                source: Some(np.source.clone()),
                source_id: flux.map(str::to_string),
                title: Some(np.title.clone()),
                artist_name: np.artist_name.clone(),
                album_title: np.album_title.clone(),
                cover_url: np.cover_path.clone(),
                duration_ms: Some(np.duration_ms).filter(|d| *d > 0),
                ..Default::default()
            })
            .await
        } else {
            self.play_from_queue_sans_historique(zone_id, queue_position)
                .await
        };
        // `play` peut rendre Ok avec un refus porté par PlayResult : ne pas
        // le perdre, pas plus qu'un Err (cible absente/ignorée, par exemple).
        let lancement = lancement.and_then(|resultat| match resultat.sortie_refusee() {
            Some(message) => Err(message.to_owned()),
            None => Ok(()),
        });
        match lancement {
            Ok(()) => {
                // Reprendre à la position de la source. Sous 3 s on repart du
                // début (même seuil que la route seek) — inutile de chercher
                // dans un flux qui vient de démarrer.
                // #4442 — pas un `seek` nu dans la foulée du `Play` : voir
                // `reprendre_la_position_transferee`. Une radio en direct n'a
                // pas de position à reprendre.
                if source_position_ms > 3000
                    && np.source != "radio"
                    && let Err(error) = self
                        .reprendre_la_position_transferee(
                            zone_id,
                            did,
                            source_position_ms,
                            !source_en_pause,
                        )
                        .await
                {
                    // Un `Seek` REFUSÉ par l'appareil (701/710/711) ne doit
                    // pas couper le transfert en son milieu : la cible joue
                    // déjà, depuis le début de la piste. Tout autre échec
                    // remonte.
                    if Self::message_deplacement_refuse(&error).is_none() {
                        return Err(error.into());
                    }
                    warn!(
                        target_zone = zone_id,
                        position_ms = source_position_ms,
                        error = %error,
                        "transfert_seek_refuse_cible_joue_depuis_le_debut"
                    );
                }
                // Une source en pause reste en pause sur la cible : transférer
                // ne veut pas dire relancer.
                if source_en_pause {
                    info!(zone_id, origine = "transfert", "pause_requested");
                    self.pause(zone_id, Some(did)).await?;
                }
            }
            Err(e) => {
                warn!(target_zone = zone_id, error = %e, "transfer_play_on_target_failed");
                return Err(ErreurDeTransfert::Lancement(e));
            }
        }
        Ok(())
    }

    /// Pose la zone cible en pause sur `np`, à `position_ms`, SANS rien
    /// envoyer à l'appareil. Faux (rien n'est posé) si la piste ne peut pas
    /// être redemandée à la reprise.
    async fn poser_le_transfert_en_pause(
        &self,
        zone_id: i64,
        did: &str,
        np: &NowPlaying,
        position_ms: u64,
    ) -> bool {
        let rejouable = if np.source == "radio" {
            np.source_id.as_deref().is_some_and(|s| !s.is_empty())
        } else {
            np.track_id.is_some() || np.source_id.as_deref().is_some_and(|s| !s.is_empty())
        };
        if !rejouable {
            return false;
        }
        // La cible jouait autre chose : elle s'arrête, comme le faisait le
        // lancement qu'on n'envoie plus.
        let cible = self.playback.get_state(zone_id).await;
        if cible.now_playing.is_some() && cible.state != PlayState::Stopped {
            self.stop(zone_id, Some(did)).await;
        }
        let mut np = np.clone();
        // La session de flux est celle de la SOURCE : la reprise en ouvrira
        // une à la cible.
        np.stream_id = None;
        self.playback.play(zone_id, np).await;
        self.playback.seek(zone_id, position_ms as i64).await;
        self.playback.pause(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "paused")
            .ok();
        self.persist_position(zone_id).await;
        self.transferts_en_pause
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(zone_id);
        info!(zone_id, position_ms, "transfert_pose_en_pause_sans_lancer");
        true
    }

    /// Vrai (une seule fois) si la zone a été posée en pause par un transfert
    /// et n'a encore rien envoyé à son appareil : sa reprise doit rétablir la
    /// piste à la position, pas envoyer un `Play` « sur place ».
    pub(super) fn prendre_le_transfert_en_pause(&self, zone_id: i64) -> bool {
        self.transferts_en_pause
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&zone_id)
    }
}

#[cfg(test)]
mod tests {
    //! Le banc : le vrai orchestrateur, une zone DLNA servie par
    //! `MockOutput` (type `dlna`), et le service « qobuz » de banc de #2095.
    //!
    //! Contre-épreuve : retirer l'appel à `poser_le_transfert_en_pause` de
    //! `transferer_sur_l_appareil` fait tomber
    //! `un_transfert_depuis_une_pause_n_envoie_rien_au_renderer` (un
    //! `play_media` part vers la cible, puis la Pause).
    use super::super::reprise_dlna_position_2095::QobuzDeBanc;
    use super::super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::play_queue_repo::PlayQueueRepo;
    use crate::db::sqlite::SqliteDb;
    use crate::db::zone_repo::ZoneRepo;
    use crate::http::streamer::AudioStreamer;
    use crate::outputs::mock::MockOutput;
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::PlaybackManager;
    use crate::streaming::ServiceRegistry;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    const APPAREIL: &str = "dlna:uuid-beosound-parents-5050";
    /// Le journal : `transfert_position_reprise position_ms=17335`.
    const POSITION_MS: u64 = 17_335;

    async fn banc() -> (PlaybackOrchestrator, i64, NowPlaying) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let mut services = ServiceRegistry::new();
        services.register(Box::new(QobuzDeBanc));
        let orch = PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(services)),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        );
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Parents", Some("dlna"), Some(APPAREIL))
            .unwrap();
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Parents").with_type("dlna"),
        ));
        // La file copiée par le transfert : une piste Qobuz.
        PlayQueueRepo::with_backend(orch.db.clone())
            .set_streaming_queue_avec_references(
                zone_id,
                &[(
                    "24293033".into(),
                    "Video Games".into(),
                    "Lana Del Rey".into(),
                    None,
                    None,
                    281_000,
                    Some("qobuz".into()),
                    Some(1),
                    Some(1),
                )],
                &[None],
                &[None],
            )
            .unwrap();
        let np = NowPlaying {
            title: "Video Games".into(),
            source: "qobuz".into(),
            source_id: Some("24293033".into()),
            stream_id: Some("session-de-la-source".into()),
            duration_ms: 281_000,
            ..Default::default()
        };
        (orch, zone_id, np)
    }

    /// `(play_media reçus, cibles des seek reçus)`.
    async fn sortie(orch: &PlaybackOrchestrator) -> (usize, Vec<u64>) {
        let arc = { orch.outputs.lock().await.get(APPAREIL) }.unwrap();
        let guard = arc.lock().await;
        let mock = guard.as_any().downcast_ref::<MockOutput>().unwrap();
        (mock.play_call_count().await, mock.seek_calls())
    }

    /// Le cas du journal : la source est en pause à 0:17. Rien ne part vers
    /// le renderer ; la cible est en pause, sur la piste, à 0:17.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn un_transfert_depuis_une_pause_n_envoie_rien_au_renderer() {
        let (orch, zone_id, np) = banc().await;
        orch.transferer_sur_l_appareil(zone_id, APPAREIL, 0, &np, POSITION_MS, true)
            .await
            .expect("le transfert aboutit");
        tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
        let (lectures, seeks) = sortie(&orch).await;
        assert_eq!(lectures, 0, "aucun SetAVTransportURI/Play vers la cible");
        assert!(seeks.is_empty(), "aucun Seek vers la cible : {seeks:?}");
        let etat = orch.playback.get_state(zone_id).await;
        assert_eq!(etat.state, PlayState::Paused);
        assert_eq!(etat.position_ms, POSITION_MS as i64);
        let posee = etat.now_playing.expect("la piste est posée");
        assert_eq!(posee.source_id.as_deref(), Some("24293033"));
        assert_eq!(posee.stream_id, None, "la session de la source ne suit pas");
    }

    /// La reprise d'une cible posée en pause : un seul lancement, puis un
    /// Seek à la position de la source — jamais un `Play` « sur place » vers
    /// un renderer qui n'a pas la piste.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn la_reprise_d_un_transfert_en_pause_relance_a_la_position() {
        let (orch, zone_id, np) = banc().await;
        orch.transferer_sur_l_appareil(zone_id, APPAREIL, 0, &np, POSITION_MS, true)
            .await
            .unwrap();
        orch.resume(zone_id, Some(APPAREIL))
            .await
            .expect("la reprise rétablit la piste");
        tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
        let (lectures, seeks) = sortie(&orch).await;
        assert_eq!(lectures, 1, "un seul lancement, à la reprise");
        assert_eq!(seeks, vec![POSITION_MS], "puis le Seek à la position");
        assert_eq!(
            orch.playback.get_state(zone_id).await.state,
            PlayState::Playing
        );
        assert!(
            !orch.prendre_le_transfert_en_pause(zone_id),
            "le constat ne sert qu'une fois"
        );
    }

    /// #6470 (fil 2302 point 16) — une RADIO transférée joue sur la cible,
    /// et non la file que la cible avait déjà. Avant : « le transfert ne se
    /// fait pas et Tune affiche juste le dernier morceau joué ».
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn une_radio_transferee_joue_le_flux_et_pas_la_file_de_la_cible_6470() {
        let (orch, zone_id, _) = banc().await;
        let radio = NowPlaying {
            title: "Radio Grenouille".into(),
            source: "radio".into(),
            source_id: Some("http://127.0.0.1:9/grenouille.mp3".into()),
            stream_id: Some("session-de-la-source".into()),
            ..Default::default()
        };
        orch.transferer_sur_l_appareil(zone_id, APPAREIL, 0, &radio, 0, false)
            .await
            .expect("le transfert aboutit");
        let titres = {
            let arc = { orch.outputs.lock().await.get(APPAREIL) }.unwrap();
            let guard = arc.lock().await;
            let mock = guard.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.play_titles().await
        };
        assert_eq!(
            titres,
            vec!["Radio Grenouille".to_string()],
            "la cible a lancé sa propre file au lieu de la radio transférée"
        );
        let joue = orch.playback.get_state(zone_id).await.now_playing.unwrap();
        assert_eq!(joue.source, "radio");
    }

    /// Une source qui JOUE garde l'ancienne conduite : la cible est lancée.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn un_transfert_en_lecture_lance_toujours_la_cible() {
        let (orch, zone_id, np) = banc().await;
        orch.transferer_sur_l_appareil(zone_id, APPAREIL, 0, &np, POSITION_MS, false)
            .await
            .unwrap();
        let (lectures, _) = sortie(&orch).await;
        assert_eq!(lectures, 1);
        assert!(!orch.prendre_le_transfert_en_pause(zone_id));
    }
}
