//! Préparer une URL avant la fin, puis adopter ce même flux sans le rouvrir.
//! La préparation ne constitue jamais une preuve de lecture (#5975/#6066).
use super::*;

type Preparation = Arc<Mutex<Option<PreparationNavigateur>>>;
pub(super) type PreparationsNavigateur = Mutex<HashMap<i64, Preparation>>;

pub(super) struct PreparationNavigateur {
    courant: String,
    sequence: u64,
    ligne: i64,
    position: i64,
    flux: ResolvedQueueItem,
}

impl PlaybackOrchestrator {
    async fn preparation_navigateur(&self, zone_id: i64) -> Preparation {
        self.browser_next
            .lock()
            .await
            .entry(zone_id)
            .or_default()
            .clone()
    }

    async fn suivante_navigateur(
        &self,
        zone_id: i64,
        courant: &str,
    ) -> Result<Option<(u64, i64, i64)>, String> {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)?
            .ok_or("zone_not_found")?;
        if zone.output_type.as_deref() != Some("browser") {
            return Err("not_a_browser_zone".into());
        }
        let etat = self.playback.get_state(zone_id).await;
        if etat.state != PlayState::Playing
            || etat
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_deref())
                != Some(courant)
        {
            return Err("browser_current_changed".into());
        }
        // Un direct ne prépare pas la vieille file laissée derrière lui.
        if etat
            .now_playing
            .as_ref()
            .is_some_and(|np| np.source == "radio")
        {
            return Ok(None);
        }
        // Le suivant manuel réserve parfois le rang avant son ouverture.
        // Refuser cette fenêtre : le flux N ne doit pas préparer N+2.
        let courante = PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, etat.queue_position)?
            .ok_or("browser_queue_changed")?;
        let np = etat.now_playing.as_ref().ok_or("browser_current_changed")?;
        if !courante.is_current
            || courante.track_id != np.track_id
            || (courante.track_id.is_none() && courante.source_id != np.source_id)
        {
            return Err("browser_queue_changed".into());
        }
        let Some(position) =
            crate::poller::PositionPoller::prochaine_position_jouable(&self.db, zone_id, &etat)
        else {
            return Ok(None);
        };
        let ligne = PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, position)?
            .ok_or("browser_queue_changed")?;
        Ok(Some((
            self.playback.current_play_seq(zone_id).await,
            ligne.id,
            position,
        )))
    }

    /// Prérequis serveur du gapless navigateur. Idempotent pour la même
    /// lecture et la même ligne de file ; ni Play, ni déplacement du curseur.
    pub async fn preparer_suivante_navigateur(
        &self,
        zone_id: i64,
        courant: &str,
    ) -> Result<Option<ResolvedQueueItem>, String> {
        // Valider la zone avant de lui réserver une entrée de préparation.
        self.suivante_navigateur(zone_id, courant).await?;
        let preparation = self.preparation_navigateur(zone_id).await;
        let mut preparations = preparation.lock().await;
        let Some((sequence, ligne, position)) = self.suivante_navigateur(zone_id, courant).await?
        else {
            *preparations = None;
            return Ok(None);
        };
        if let Some(p) = preparations.as_ref()
            && p.courant == courant
            && p.sequence == sequence
            && p.ligne == ligne
            && p.position == position
            && self.flux_pre_arme(zone_id).await == p.flux.stream_id
            && self
                .streamer
                .session_alive(p.flux.stream_id.as_deref().unwrap_or_default())
                .await
        {
            return Ok(Some(p.flux.clone()));
        }
        *preparations = None;
        let flux = self.resolve_queue_item_url(zone_id, position).await?;
        // Résoudre peut attendre un fichier ou le réseau : ne pas annoncer
        // une URL devenue étrangère à la lecture ou à la file entre-temps.
        let apres = self.suivante_navigateur(zone_id, courant).await;
        if apres.as_ref().ok().copied().flatten() != Some((sequence, ligne, position)) {
            // La commande manuelle reste libre pendant la résolution. Ne pas
            // laisser son ancien préchargement consommer réseau ou fichier.
            if self.flux_pre_arme(zone_id).await == flux.stream_id {
                self.cleanup_gapless_session(zone_id).await;
            }
            return Err(apres
                .err()
                .unwrap_or_else(|| "browser_queue_changed".into()));
        }
        if flux.stream_id.is_none() {
            return Err("browser_next_without_stream".into());
        }
        *preparations = Some(PreparationNavigateur {
            courant: courant.into(),
            sequence,
            ligne,
            position,
            flux: flux.clone(),
        });
        Ok(Some(flux))
    }

    /// Le client appelle après le début EFFECTIF du flux préchargé, avec une
    /// position positive de son horloge. Une sonde HTTP ne suffit pas.
    pub async fn adopter_suivante_navigateur(
        &self,
        zone_id: i64,
        courant: &str,
        suivant: &str,
        position_ms: i64,
    ) -> Result<(), String> {
        if position_ms <= 0 {
            return Err("browser_next_not_playing".into());
        }
        // Valider la zone avant de lui réserver une entrée de préparation.
        self.suivante_navigateur(zone_id, courant).await?;
        let preparation = self.preparation_navigateur(zone_id).await;
        let mut preparations = preparation.lock().await;
        let p = preparations.as_ref().ok_or("browser_next_not_prepared")?;
        if p.courant != courant
            || p.flux.stream_id.as_deref() != Some(suivant)
            || self.flux_pre_arme(zone_id).await.as_deref() != Some(suivant)
            || self.suivante_navigateur(zone_id, courant).await?
                != Some((p.sequence, p.ligne, p.position))
        {
            return Err("browser_queue_changed".into());
        }
        self.advance_queue_metadata(zone_id, p.position).await?;
        *preparations = None;
        self.signaler_lecture_navigateur(zone_id, suivant, position_ms)
            .await;
        Ok(())
    }
}
